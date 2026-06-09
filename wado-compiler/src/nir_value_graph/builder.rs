//! Per-function ValueGraph builder.
//!
//! Walks the SkelTree (`Body`) once and assigns a [`ValueId`] to every pure
//! [`ExprId`]. Impure or allocation-bearing expressions (calls, struct/array
//! literals, control flow, etc.) get no entry in `value_of`; their Skel
//! position carries the value at extraction time, not a ValueGraph node.
//!
//! See `docs/wep-2026-06-05-worklist-rewrite-engine.md` (Stage 2). The
//! ValueGraph this builder produces is not yet consumed by any rule —
//! Stages 3 – 6 migrate the existing passes onto it.
//!
//! # Flow handling (MVP)
//!
//! - Function parameters are seeded with a fresh `Opaque` value each.
//! - `Let` / `Assign`-to-bare-`Local` updates `current_value[local_idx]` to
//!   the RHS's value (or `Opaque` if the RHS is impure).
//! - `If` snapshots `current_value`, walks both branches, then merges: for
//!   each local that diverged across the branches we hash-cons a
//!   [`ValueKind::Select`] keyed on the condition's value. If the condition
//!   is impure (no ValueGraph id), the merge falls back to a fresh
//!   `Opaque`.
//! - `Match` / `Switch` walk every arm and merge n-ary: if every arm agrees
//!   on a local, that value carries; otherwise the local goes `Opaque`. We
//!   do not yet construct n-ary `Select` chains for them — that is Stage 6
//!   territory (induction recognition + bound implication need the same
//!   machinery and are tackled together).
//! - `Loop` pre-scans the body for locals it may write and reassigns each
//!   to a fresh `Opaque` before walking the body; post-loop those locals
//!   stay `Opaque`. Stage 6 swaps recurring-pattern locals to `LoopPhi`.
//! - `LabeledBlock` walks as a regular block. `Break` flow-merging into the
//!   label target is conservative — locals modified inside that escape via
//!   `Break` end up `Opaque` after the labeled block.
//! - Pattern bindings (`LetDestructure`, `Match` arm bindings) are seeded
//!   with `Opaque`. Field destructuring will become real `FieldAccess` /
//!   `VariantPayload` Value kinds once heap-version tracking activates in
//!   Stage 5.

use crate::hashmap::IndexMap;
use crate::nir::{NirParam, NirUnaryOp};
use crate::nir_arena::{
    ArmData, Body, ExprId, ExprKind, NodeRef, PatId, PatKind, StmtId, StmtKind,
};

use super::{ValueId, ValuePool};

/// The result of running [`build`] over a function body: the populated
/// pool plus the side-table mapping pure `ExprId`s to their `ValueId`.
///
/// Impure positions are absent from `value_of`. A rule that wants "this
/// expression's value" should look it up and treat the absence as "no
/// value-graph identity available" rather than panicking.
#[derive(Debug)]
pub struct ValueGraphBuild {
    pub pool: ValuePool,
    pub value_of: IndexMap<ExprId, ValueId>,
}

/// Build the ValueGraph for one function body.
///
/// `params` seed `current_value` with one fresh `Opaque` per parameter, so a
/// `Local { index: param.local_index }` read returns that Opaque every time
/// until the parameter is reassigned (which the builder picks up the same
/// way as any other `Assign`).
pub fn build(body: &Body, params: &[NirParam]) -> ValueGraphBuild {
    let mut b = Builder::new(body);
    b.seed_params(params);
    b.walk_block(body.root);
    ValueGraphBuild {
        pool: b.pool,
        value_of: b.value_of,
    }
}

struct Builder<'a> {
    body: &'a Body,
    pool: ValuePool,
    value_of: IndexMap<ExprId, ValueId>,
    /// `local_index → current Value` at the current program point. Cloned at
    /// branch entries so each arm walks from the pre-branch snapshot.
    current_value: IndexMap<u32, ValueId>,
}

impl<'a> Builder<'a> {
    fn new(body: &'a Body) -> Self {
        Self {
            body,
            pool: ValuePool::new(),
            value_of: IndexMap::default(),
            current_value: IndexMap::default(),
        }
    }

    fn seed_params(&mut self, params: &[NirParam]) {
        for param in params {
            let opaque = self.pool.fresh_opaque();
            self.current_value.insert(param.local_index, opaque);
        }
    }

    // ------------------------------------------------------------------
    // Blocks / statements
    // ------------------------------------------------------------------

    fn walk_block(&mut self, block: crate::nir_arena::BlockId) {
        let stmts = self.body.blocks[block].stmts.clone();
        for s in stmts {
            self.walk_stmt(s);
        }
    }

    fn walk_stmt(&mut self, stmt: StmtId) {
        match self.body.stmts[stmt].kind.clone() {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let v = self
                    .walk_expr(value)
                    .unwrap_or_else(|| self.pool.fresh_opaque());
                self.current_value.insert(local_index, v);
            }
            StmtKind::LetDestructure { pattern, value, .. } => {
                self.walk_expr(value);
                // MVP: destructured bindings are Opaque. Field-projection
                // Value kinds will activate alongside `FieldAccess` in
                // Stage 5.
                self.bind_pattern_opaque(pattern);
            }
            StmtKind::Expr(e) => {
                self.walk_expr(e);
            }
            StmtKind::Return { value } => {
                if let Some(v) = value {
                    self.walk_expr(v);
                }
            }
            StmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.walk_expr(v);
                }
            }
            StmtKind::Continue => {}
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let cond_v = self.walk_expr(condition);
                let saved = self.current_value.clone();
                self.walk_block(then_block);
                let then_state = std::mem::replace(&mut self.current_value, saved.clone());
                if let Some(eb) = else_block {
                    self.walk_block(eb);
                }
                let else_state = std::mem::replace(&mut self.current_value, saved.clone());
                self.merge_two_arms(cond_v, &saved, &then_state, &else_state);
            }
            StmtKind::Loop { body: lb } => {
                self.walk_loop(lb);
            }
            StmtKind::LabeledBlock { block, .. } => {
                // Conservative: locals modified inside that may escape via
                // `break` could disagree with the fall-through value. For
                // MVP, take the snapshot before, walk, then any local that
                // changed becomes Opaque after.
                let saved = self.current_value.clone();
                self.walk_block(block);
                self.dirty_changed_locals(&saved);
            }
        }
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    /// Walk `expr` and return its ValueGraph id if the expression is pure.
    /// Impure expressions return `None`; their pure children are still walked
    /// for their side of the side-table.
    fn walk_expr(&mut self, expr: ExprId) -> Option<ValueId> {
        let id = self.compute_value(expr);
        if let Some(id) = id {
            self.value_of.insert(expr, id);
        }
        id
    }

    fn compute_value(&mut self, expr: ExprId) -> Option<ValueId> {
        match self.body.exprs[expr].kind.clone() {
            // ---- Literals ----
            ExprKind::IntLiteral { value, .. } => Some(self.pool.int(value)),
            ExprKind::FloatLiteral { value, .. } => Some(self.pool.float(value)),
            ExprKind::BoolLiteral(b) => Some(self.pool.bool(b)),
            ExprKind::CharLiteral(c) => Some(self.pool.char(c)),
            ExprKind::StringLiteral(s) => Some(self.pool.string(s)),
            ExprKind::Null => Some(self.pool.null()),
            ExprKind::Unit => Some(self.pool.unit()),

            // ---- Local read ----
            ExprKind::Local { index, .. } => Some(self.read_local(index)),

            // ---- Pure arithmetic ----
            ExprKind::Binary { left, op, right } => {
                let lhs = self.walk_expr(left)?;
                let rhs = self.walk_expr(right)?;
                Some(self.pool.binary(op, lhs, rhs))
            }
            ExprKind::Unary { op, expr: inner } => {
                // `Ref` / `MutRef` / `Deref` are address-taking / heap-bearing
                // operations — not pure values. Walk the child (so pure
                // subtrees still land in `value_of`) but do not assign an id
                // to this expr.
                if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref) {
                    self.walk_expr(inner);
                    None
                } else {
                    let operand = self.walk_expr(inner)?;
                    Some(self.pool.unary(op, operand))
                }
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let operand = self.walk_expr(inner)?;
                Some(self.pool.cast(operand, target_type))
            }

            // ---- Mutation: side-effect, never pure ----
            ExprKind::Assign { target, value } => {
                let v = self
                    .walk_expr(value)
                    .unwrap_or_else(|| self.pool.fresh_opaque());
                // Bare `Local` target = local reassignment. Anything else
                // (FieldAccess, Index, Unary::Deref) is a heap write — Stage
                // 5 will bump heap_version here; for MVP we just walk the
                // target's children.
                match &self.body.exprs[target].kind {
                    ExprKind::Local { index, .. } => {
                        self.current_value.insert(*index, v);
                    }
                    _ => {
                        self.walk_expr(target);
                    }
                }
                None
            }
            ExprKind::GlobalVarSet { value, .. } => {
                self.walk_expr(value);
                None
            }

            // ---- Control-flow expressions ----
            ExprKind::Block(block) => {
                // Walk every stmt for the side-table; the block expr itself
                // is not currently Value-graph-able (Stage 6 may revisit for
                // `Match`-replacement Selects).
                self.walk_block(block);
                None
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_v = self.walk_expr(condition);
                let saved = self.current_value.clone();
                self.walk_block(then_branch);
                let then_state = std::mem::replace(&mut self.current_value, saved.clone());
                if let Some(eb) = else_branch {
                    self.walk_block(eb);
                }
                let else_state = std::mem::replace(&mut self.current_value, saved.clone());
                self.merge_two_arms(cond_v, &saved, &then_state, &else_state);
                None
            }
            ExprKind::LabeledBlock { block, .. } => {
                let saved = self.current_value.clone();
                self.walk_block(block);
                self.dirty_changed_locals(&saved);
                None
            }
            ExprKind::Match { expr: scrut, arms } => {
                self.walk_expr(scrut);
                self.walk_match_arms(&arms);
                None
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.walk_expr(scrutinee);
                let saved = self.current_value.clone();
                let mut arm_states: Vec<IndexMap<u32, ValueId>> =
                    Vec::with_capacity(arms.len() + 1);
                for arm in &arms {
                    self.current_value = saved.clone();
                    self.walk_block(*arm);
                    arm_states.push(self.current_value.clone());
                }
                self.current_value = saved.clone();
                self.walk_block(default);
                arm_states.push(std::mem::replace(&mut self.current_value, saved.clone()));
                self.merge_n_arms(&saved, &arm_states);
                None
            }

            // ---- Heap-bearing reads (MVP: Skel-side, no Value id) ----
            ExprKind::FieldAccess { expr: inner, .. } => {
                // Stage 5 activates `ValueKind::FieldAccess` with heap-version
                // tracking. For now the receiver is walked but the read itself
                // gets no id.
                self.walk_expr(inner);
                None
            }
            ExprKind::Index { expr: inner, index } => {
                self.walk_expr(inner);
                self.walk_expr(index);
                None
            }
            ExprKind::VariantTag { expr: inner }
            | ExprKind::VariantTest { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. } => {
                self.walk_expr(inner);
                None
            }

            // ---- Allocation-bearing constructors (Skel-side per Q1) ----
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.walk_expr(f.value);
                }
                None
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for e in elements {
                    self.walk_expr(e);
                }
                None
            }
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.walk_expr(p);
                }
                None
            }
            ExprKind::EnumConstruct { .. } => None,
            ExprKind::ClosureToCanonical { functor, .. } => {
                self.walk_expr(functor);
                None
            }

            // ---- Calls (effectful) ----
            ExprKind::Call { args, .. } => {
                for a in args {
                    self.walk_expr(a.expr);
                }
                None
            }
            ExprKind::CmRawCall { args, .. } => {
                for a in args {
                    self.walk_expr(a);
                }
                None
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                for a in args {
                    self.walk_expr(a.expr);
                }
                None
            }
            ExprKind::IndirectCall { callee, args } => {
                self.walk_expr(callee);
                for a in args {
                    self.walk_expr(a);
                }
                None
            }

            // ---- Other Skel-side leaves ----
            ExprKind::GlobalVarGet { .. } | ExprKind::BytesLiteral(_) => None,
        }
    }

    fn read_local(&mut self, idx: u32) -> ValueId {
        if let Some(&v) = self.current_value.get(&idx) {
            v
        } else {
            // First read of a local not yet bound by Let / param seeding —
            // shouldn't happen on well-typed NIR, but stay graceful: emit an
            // Opaque and record it so subsequent reads agree.
            let v = self.pool.fresh_opaque();
            self.current_value.insert(idx, v);
            v
        }
    }

    // ------------------------------------------------------------------
    // Pattern bindings
    // ------------------------------------------------------------------

    fn bind_pattern_opaque(&mut self, pat: PatId) {
        match self.body.pats[pat].kind.clone() {
            PatKind::Binding { local_index, .. } => {
                let v = self.pool.fresh_opaque();
                self.current_value.insert(local_index, v);
            }
            PatKind::Tuple(children, _) => {
                for c in children {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Or(children) => {
                for c in children {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Variant { bindings, .. } => {
                for c in bindings {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Struct { fields, .. } => {
                for f in fields {
                    self.bind_pattern_opaque(f.pattern);
                }
            }
            PatKind::ConstantValue { expr } => {
                self.walk_expr(expr);
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::Range { .. } => {}
        }
    }

    // ------------------------------------------------------------------
    // Control flow: branch merge and loop handling
    // ------------------------------------------------------------------

    /// Merge an if-style two-arm structural endpoint. For each local that
    /// existed before the branch:
    /// - Both arms agree → keep that value.
    /// - Arms differ AND `cond_v` is known → hash-cons a `Select`.
    /// - Otherwise → fresh `Opaque` (the merged value is unknown).
    ///
    /// New bindings introduced inside a single arm are dropped from the
    /// post-merge state — they are scoped to the arm in well-formed NIR.
    fn merge_two_arms(
        &mut self,
        cond_v: Option<ValueId>,
        saved: &IndexMap<u32, ValueId>,
        then_state: &IndexMap<u32, ValueId>,
        else_state: &IndexMap<u32, ValueId>,
    ) {
        for (&idx, &saved_v) in saved {
            let then_v = then_state.get(&idx).copied().unwrap_or(saved_v);
            let else_v = else_state.get(&idx).copied().unwrap_or(saved_v);
            let merged = if then_v == else_v {
                then_v
            } else if let Some(cond) = cond_v {
                self.pool.select(cond, then_v, else_v)
            } else {
                self.pool.fresh_opaque()
            };
            self.current_value.insert(idx, merged);
        }
    }

    /// Merge n arms (Match / Switch). For each pre-branch local: if every
    /// arm agrees, keep that value; otherwise fall back to `Opaque`. We do
    /// not currently build n-ary `Select` chains — Stage 6 introduces the
    /// machinery for that alongside induction recognition.
    fn merge_n_arms(
        &mut self,
        saved: &IndexMap<u32, ValueId>,
        arm_states: &[IndexMap<u32, ValueId>],
    ) {
        for (&idx, &saved_v) in saved {
            let mut consensus: Option<ValueId> = None;
            let mut diverged = false;
            for arm in arm_states {
                let arm_v = arm.get(&idx).copied().unwrap_or(saved_v);
                match consensus {
                    None => consensus = Some(arm_v),
                    Some(c) if c == arm_v => {}
                    Some(_) => {
                        diverged = true;
                        break;
                    }
                }
            }
            let merged = if diverged {
                self.pool.fresh_opaque()
            } else {
                consensus.unwrap_or(saved_v)
            };
            self.current_value.insert(idx, merged);
        }
    }

    fn walk_match_arms(&mut self, arms: &[ArmData]) {
        let saved = self.current_value.clone();
        let mut states: Vec<IndexMap<u32, ValueId>> = Vec::with_capacity(arms.len());
        for arm in arms {
            self.current_value = saved.clone();
            // Pattern bindings introduced by this arm are conservatively
            // Opaque. The arm body and guard are walked from that state.
            self.bind_pattern_opaque(arm.pattern);
            if let Some(g) = arm.guard {
                self.walk_expr(g);
            }
            self.walk_expr(arm.body);
            states.push(self.current_value.clone());
        }
        self.current_value = saved.clone();
        self.merge_n_arms(&saved, &states);
    }

    /// Pre-scan a loop body for locals it may write and reassign each to a
    /// fresh `Opaque` before walking. Locals untouched by the body keep
    /// their pre-loop values. Post-loop, the modified locals are again
    /// `Opaque` — the body may have run 0..N times and we cannot describe
    /// the final value without a `LoopPhi`. (MVP — Stage 6 swaps in
    /// `LoopPhi` for recognisable inductions.)
    fn walk_loop(&mut self, body_block: crate::nir_arena::BlockId) {
        let mut writes: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        collect_writes_in_block(self.body, body_block, &mut writes);
        for idx in &writes {
            // Locals not yet in `current_value` (e.g., declared inside the
            // loop) get fresh Opaques as part of walking the body; we don't
            // need to pre-seed those.
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        self.walk_block(body_block);
        // After the body walks, an in-loop `Assign` may have overwritten a
        // pre-seeded Opaque with a derived value (e.g., `i = i + 1` writes
        // `Binary(Add, OpaqueEntry, Int 1)`). The post-loop value is still
        // unknown — there could have been 0, 1, or many iterations — so
        // reset modified locals to a fresh Opaque again.
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
    }

    /// After a flow-opaque construct (LabeledBlock with potential breaks),
    /// any local that the construct changed becomes Opaque. Untouched locals
    /// keep their saved value.
    fn dirty_changed_locals(&mut self, saved: &IndexMap<u32, ValueId>) {
        let to_dirty: Vec<u32> = self
            .current_value
            .iter()
            .filter_map(|(&idx, &v)| {
                let s = saved.get(&idx)?;
                if *s != v { Some(idx) } else { None }
            })
            .collect();
        for idx in to_dirty {
            let opaque = self.pool.fresh_opaque();
            self.current_value.insert(idx, opaque);
        }
    }
}

/// Collect every local index that an `Assign`-to-bare-`Local`, a `Let`, or
/// a `LetDestructure` binding writes anywhere in `block`'s subtree. Used by
/// `walk_loop` to identify the locals it must reset to `Opaque` at loop
/// entry.
fn collect_writes_in_block(
    body: &Body,
    block: crate::nir_arena::BlockId,
    out: &mut crate::hashmap::IndexSet<u32>,
) {
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        collect_writes_in_stmt(body, s, out);
    }
}

fn collect_writes_in_stmt(body: &Body, stmt: StmtId, out: &mut crate::hashmap::IndexSet<u32>) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            out.insert(*local_index);
            collect_writes_in_expr(body, *value, out);
        }
        StmtKind::LetDestructure { pattern, value, .. } => {
            collect_writes_in_pattern(body, *pattern, out);
            collect_writes_in_expr(body, *value, out);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
                    NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out),
                    NodeRef::Block(b) => collect_writes_in_block(body, b, out),
                    NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
                }
            }
        }
    }
}

fn collect_writes_in_expr(body: &Body, expr: ExprId, out: &mut crate::hashmap::IndexSet<u32>) {
    if let ExprKind::Assign { target, .. } = &body.exprs[expr].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
    {
        out.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(expr), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
            NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out),
            NodeRef::Block(b) => collect_writes_in_block(body, b, out),
            NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
        }
    }
}

fn collect_writes_in_pattern(body: &Body, pat: PatId, out: &mut crate::hashmap::IndexSet<u32>) {
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            out.insert(*local_index);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Pat(pat), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
                    NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirParam};
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::nir_value_graph::ValueKind;
    use crate::tir::TypeTable;
    use crate::token::Span;

    // ----- Body builders for tests -----

    fn empty_body() -> Body {
        Body::empty()
    }

    fn alloc_expr(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeTable::UNIT,
            span: Span::default(),
        })
    }

    fn alloc_stmt(body: &mut Body, kind: StmtKind) -> StmtId {
        body.stmts.push(StmtNode {
            kind,
            span: Span::default(),
        })
    }

    fn root_with(body: &mut Body, stmts: Vec<StmtId>) {
        body.root = body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        });
    }

    fn int_lit(body: &mut Body, value: u64) -> ExprId {
        alloc_expr(
            body,
            ExprKind::IntLiteral {
                value,
                repr: value.to_string(),
            },
        )
    }

    fn local_ref(body: &mut Body, idx: u32) -> ExprId {
        alloc_expr(
            body,
            ExprKind::Local {
                index: idx,
                name: format!("__l{idx}"),
            },
        )
    }

    fn let_stmt(body: &mut Body, idx: u32, value: ExprId, is_mut: bool) -> StmtId {
        alloc_stmt(
            body,
            StmtKind::Let {
                name: format!("__l{idx}"),
                local_index: idx,
                is_mut,
                is_reactive: false,
                type_id: TypeTable::UNIT,
                value,
                skip_value_copy: false,
            },
        )
    }

    fn assign_stmt(body: &mut Body, idx: u32, value: ExprId) -> StmtId {
        let target = local_ref(body, idx);
        let assign = alloc_expr(body, ExprKind::Assign { target, value });
        alloc_stmt(body, StmtKind::Expr(assign))
    }

    fn binary(body: &mut Body, op: NirBinaryOp, left: ExprId, right: ExprId) -> ExprId {
        alloc_expr(body, ExprKind::Binary { left, op, right })
    }

    fn bool_lit(body: &mut Body, b: bool) -> ExprId {
        alloc_expr(body, ExprKind::BoolLiteral(b))
    }

    fn block_with(body: &mut Body, stmts: Vec<StmtId>) -> crate::nir_arena::BlockId {
        body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        })
    }

    // ----- Tests -----

    #[test]
    fn literal_int_gets_value_id() {
        let mut body = empty_body();
        let lit = int_lit(&mut body, 42);
        let s = alloc_stmt(&mut body, StmtKind::Expr(lit));
        root_with(&mut body, vec![s]);
        let r = build(&body, &[]);
        let v = r.value_of[&lit];
        assert_eq!(r.pool.kind(v), &ValueKind::Int(42));
    }

    #[test]
    fn let_then_read_returns_same_value() {
        // let x = 1; x
        let mut body = empty_body();
        let lit = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, lit, false);
        let read = local_ref(&mut body, 0);
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, s2]);
        let r = build(&body, &[]);
        let lit_v = r.value_of[&lit];
        let read_v = r.value_of[&read];
        assert_eq!(lit_v, read_v);
        assert_eq!(r.pool.kind(read_v), &ValueKind::Int(1));
    }

    #[test]
    fn equivalent_arithmetic_dedupes() {
        // let a = 1 + 2; let b = 1 + 2;
        let mut body = empty_body();
        let one_a = int_lit(&mut body, 1);
        let two_a = int_lit(&mut body, 2);
        let add_a = binary(&mut body, NirBinaryOp::Add, one_a, two_a);
        let let_a = let_stmt(&mut body, 0, add_a, false);
        let one_b = int_lit(&mut body, 1);
        let two_b = int_lit(&mut body, 2);
        let add_b = binary(&mut body, NirBinaryOp::Add, one_b, two_b);
        let let_b = let_stmt(&mut body, 1, add_b, false);
        root_with(&mut body, vec![let_a, let_b]);
        let r = build(&body, &[]);
        assert_eq!(r.value_of[&add_a], r.value_of[&add_b]);
    }

    #[test]
    fn reassignment_updates_local_value() {
        // let mut x = 1; x = 2; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let two = int_lit(&mut body, 2);
        let assign = assign_stmt(&mut body, 0, two);
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, assign, s_read]);
        let r = build(&body, &[]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(2));
    }

    #[test]
    fn if_merge_builds_select() {
        // let mut x = 1; if true { x = 2; } else { x = 3; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two);
        let then_block = block_with(&mut body, vec![assign_then]);
        let three = int_lit(&mut body, 3);
        let assign_else = assign_stmt(&mut body, 0, three);
        let else_block = block_with(&mut body, vec![assign_else]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build(&body, &[]);
        let read_v = r.value_of[&read];
        match r.pool.kind(read_v) {
            ValueKind::Select { cond, then, else_ } => {
                assert_eq!(r.pool.kind(*cond), &ValueKind::Bool(true));
                assert_eq!(r.pool.kind(*then), &ValueKind::Int(2));
                assert_eq!(r.pool.kind(*else_), &ValueKind::Int(3));
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn if_without_else_merges_with_pre_value() {
        // let mut x = 1; if true { x = 2; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two);
        let then_block = block_with(&mut body, vec![assign_then]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: None,
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build(&body, &[]);
        match r.pool.kind(r.value_of[&read]) {
            ValueKind::Select { cond, then, else_ } => {
                assert_eq!(r.pool.kind(*cond), &ValueKind::Bool(true));
                assert_eq!(r.pool.kind(*then), &ValueKind::Int(2));
                assert_eq!(r.pool.kind(*else_), &ValueKind::Int(1));
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn if_with_agreeing_arms_skips_select() {
        // let mut x = 1; if true { x = 2; } else { x = 2; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two_a = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two_a);
        let then_block = block_with(&mut body, vec![assign_then]);
        let two_b = int_lit(&mut body, 2);
        let assign_else = assign_stmt(&mut body, 0, two_b);
        let else_block = block_with(&mut body, vec![assign_else]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build(&body, &[]);
        // Both arms wrote Int(2); the merge picks that without a Select.
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(2));
    }

    #[test]
    fn loop_modified_local_becomes_opaque() {
        // let mut i = 0; loop { i = i + 1; }; i
        let mut body = empty_body();
        let zero = int_lit(&mut body, 0);
        let let_s = let_stmt(&mut body, 0, zero, true);
        let i_read = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read, one);
        let assign = assign_stmt(&mut body, 0, plus);
        let lb = block_with(&mut body, vec![assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, loop_s, s_read]);
        let r = build(&body, &[]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Opaque(_)
        ));
    }

    #[test]
    fn loop_unmodified_local_keeps_value() {
        // let x = 1; let mut i = 0; loop { i = i + 1; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_x = let_stmt(&mut body, 0, one, false);
        let zero = int_lit(&mut body, 0);
        let let_i = let_stmt(&mut body, 1, zero, true);
        let i_read = local_ref(&mut body, 1);
        let lit_one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read, lit_one);
        let assign = assign_stmt(&mut body, 1, plus);
        let lb = block_with(&mut body, vec![assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_x, let_i, loop_s, s_read]);
        let r = build(&body, &[]);
        // `x` is not touched by the loop, so it retains its Int(1).
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(1));
    }

    #[test]
    fn function_parameter_is_opaque_and_reads_dedup() {
        // fn(x: i32) { x; x; }
        let mut body = empty_body();
        let read1 = local_ref(&mut body, 0);
        let read2 = local_ref(&mut body, 0);
        let s1 = alloc_stmt(&mut body, StmtKind::Expr(read1));
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(read2));
        root_with(&mut body, vec![s1, s2]);
        let param = NirParam {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build(&body, &[param]);
        let v1 = r.value_of[&read1];
        let v2 = r.value_of[&read2];
        assert_eq!(v1, v2);
        assert!(matches!(r.pool.kind(v1), ValueKind::Opaque(_)));
    }

    #[test]
    fn binary_on_param_reads_dedupes() {
        // fn(x: i32) { x + 1; x + 1; }
        let mut body = empty_body();
        let read1 = local_ref(&mut body, 0);
        let one_a = int_lit(&mut body, 1);
        let add_a = binary(&mut body, NirBinaryOp::Add, read1, one_a);
        let read2 = local_ref(&mut body, 0);
        let one_b = int_lit(&mut body, 1);
        let add_b = binary(&mut body, NirBinaryOp::Add, read2, one_b);
        let s1 = alloc_stmt(&mut body, StmtKind::Expr(add_a));
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(add_b));
        root_with(&mut body, vec![s1, s2]);
        let param = NirParam {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build(&body, &[param]);
        // Both `x + 1` expressions share a ValueId because `x` is a stable
        // (write-once) Opaque and `1` is hash-consed.
        assert_eq!(r.value_of[&add_a], r.value_of[&add_b]);
    }

    #[test]
    fn impure_let_value_gets_opaque_binding() {
        // let x = call(); x
        // We synthesise a Call node here; the builder treats it as impure so
        // `x` gets a fresh Opaque rather than tracking the Call's value.
        use crate::module_source::ModuleSource;
        use crate::nir::FunctionRef;
        let mut body = empty_body();
        let call = alloc_expr(
            &mut body,
            ExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::entry_point_synthetic(),
                    name: "foo".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![],
            },
        );
        let let_s = let_stmt(&mut body, 0, call, false);
        let read = local_ref(&mut body, 0);
        let s = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, s]);
        let r = build(&body, &[]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Opaque(_)
        ));
        // The call itself has no value_of entry.
        assert!(!r.value_of.contains_key(&call));
    }
}
