//! Extraction: materialize pure values from the live ValueGraph back into the
//! SkelTree.
//!
//! In the live-ValueGraph design (see
//! `docs/wep-2026-06-15-live-value-graph.md`) pure-value rewrites prove
//! equivalences by unioning e-classes rather than editing the skeleton. A read
//! of an expression's value therefore goes through the class representative
//! ([`Engine::value_find`]); the extractor walks the skeleton and lowers each
//! pure operand to the representative's chosen concrete form.
//!
//! This is the literal case — the keystone every union-based pure-value rule
//! reuses: an expression whose representative is a constant (`Int` / `Float` /
//! `Bool` / `Char`) is replaced by that literal. The share-vs-duplicate cost
//! model for non-literal multi-use values is a later step; constants are always
//! cheaper to rematerialize than to share, so they need no cost decision.
//!
//! [`materialize_literal`] is the shared materialization primitive, used in
//! production by `store_load_forward`. [`ExtractLiteralRule`] (the worklist
//! rule form) is exercised by unit tests until the first union-producing
//! pure-value pass wires it into a combined session.
#![allow(dead_code)]

use crate::nir_arena::{ExprId, ExprKind, NodeRef, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::{ValueId, ValueKind};

/// Rewrite a pure expression whose ValueGraph representative is a literal into
/// that literal. Idempotent: an expression already holding the target literal
/// is left untouched, so the worklist retry terminates.
pub(super) struct ExtractLiteralRule;

impl Rule for ExtractLiteralRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        // An assign target is a place, not a value — never materialize it.
        if is_assign_target(e, id) {
            return false;
        }
        let Some(vid) = e.value(id) else {
            return false;
        };
        let rep = e.value_find(vid);
        let Some(value) = extract_const(e, rep, id) else {
            return false;
        };
        // Promote the node to the pooled constant in its parent slot (WEP: The
        // Live ValueGraph). Idempotent: a promoted (orphaned) node has no parent
        // slot, so the retry reports no change and the worklist terminates.
        e.replace_expr_with_value(id, value)
    }
}

/// True if `id` is the value operand of a `let` binding. Freezing it would
/// replace `let x = <arith>` with `let x = Operand::Value`, which the WIR
/// builder lowers without the `LocalSet x (value)` shape the `local.tee`
/// fusion (and other `LocalSet`-keyed WIR peepholes) match on — a code-quality
/// regression. Leave the binding's value in the skeleton; freeze arith in other
/// positions (args, returns, sub-expressions) instead.
fn is_let_value(e: &Engine, id: ExprId) -> bool {
    matches!(
        e.parent_of(NodeRef::Expr(id)),
        Some(NodeRef::Stmt(s))
            if matches!(&e.body.stmts[s].kind,
                StmtKind::Let { value, .. } if value.as_expr() == Some(id))
    )
}

/// True if `id` is a pure-arith node (`Binary` / `Cast` / pure `Unary`) — a
/// freeze candidate. Under early promotion, `FieldAccess` reads also qualify
/// (their value re-emits as a `StructGet`; reemittability + the shared `heap_ver`
/// keep it sound), extending promotion toward every pure value (WEP P2).
fn is_pure_arith(e: &Engine, id: ExprId, include_fields: bool) -> bool {
    matches!(
        &e.body.exprs[id].kind,
        ExprKind::Binary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Unary {
                op: crate::nir::NirUnaryOp::Neg
                    | crate::nir::NirUnaryOp::Not
                    | crate::nir::NirUnaryOp::BitNot,
                ..
            }
    ) || (include_fields && matches!(&e.body.exprs[id].kind, ExprKind::FieldAccess { .. }))
}

/// The statement that directly encloses `expr`, and that statement's block —
/// walking up the parent chain to the first `Stmt`. Used to insert a source-point
/// materialisation `let` just before the use's statement (WEP P2).
fn enclosing_stmt_and_block(
    e: &Engine,
    expr: ExprId,
) -> Option<(crate::nir_arena::StmtId, crate::nir_arena::BlockId)> {
    let mut node = NodeRef::Expr(expr);
    loop {
        match e.parent_of(node)? {
            NodeRef::Stmt(s) => {
                let NodeRef::Block(b) = e.parent_of(NodeRef::Stmt(s))? else {
                    return None;
                };
                return Some((s, b));
            }
            other => node = other,
        }
    }
}

/// The statement before which a `let _av = <FieldAccess value>` materialising the
/// uses `ids` can be inserted: the **earliest** of their enclosing statements,
/// when every use shares one enclosing block. A single block of straight-line
/// statements is a dominance chain — the earliest statement precedes (and so
/// dominates) the rest — and the uses share one `ValueId`, so no heap bump
/// separates them: the field's version is constant across the span, and a load
/// pinned at the earliest use reproduces it for all. Cross-block uses need real
/// dominance and are deferred (`None`).
fn shared_field_materialise_point(
    e: &Engine,
    ids: &[ExprId],
) -> Option<(crate::nir_arena::StmtId, crate::nir_arena::BlockId)> {
    let mut block: Option<crate::nir_arena::BlockId> = None;
    let mut stmts: Vec<crate::nir_arena::StmtId> = Vec::with_capacity(ids.len());
    for &id in ids {
        let (s, b) = enclosing_stmt_and_block(e, id)?;
        match block {
            None => block = Some(b),
            Some(bb) if bb == b => {}
            _ => return None,
        }
        stmts.push(s);
    }
    let b = block?;
    let order = &e.body.blocks[b].stmts;
    let earliest = *stmts
        .iter()
        .min_by_key(|s| order.iter().position(|x| x == *s).unwrap_or(usize::MAX))?;
    Some((earliest, b))
}

/// Stamp `type_id` onto `v` and its arithmetic children so the WIR extractor
/// can recover each value's width. Arithmetic is width-uniform, so the result
/// type carries down through `Binary` / `Unary`. Returns `false` on a **width
/// conflict**: `ValueKind` is type-erased and hash-consed, so a value shared
/// between two differently-typed uses (e.g. `a+b` as both `i32` and `i64`, or a
/// `0.0` literal as `f32` and `f64`) already carries the other use's type —
/// freezing this use would extract it at the wrong width, so the caller skips
/// it. (`Cast` never appears — excluded by `value_fully_reemittable_locally`.)
#[must_use]
fn record_value_tree_types(e: &mut Engine, v: ValueId, type_id: crate::tir::TypeId) -> bool {
    use crate::nir_value_graph::ValueKind;
    let rep = e.body.values.find(v);
    match e.body.values.type_of(rep) {
        Some(existing) if existing != type_id => return false,
        Some(_) => {}
        None => e.body.values.set_type(rep, type_id),
    }
    match e.body.values.kind(rep).clone() {
        ValueKind::Binary { lhs, rhs, .. } => {
            record_value_tree_types(e, lhs, type_id) && record_value_tree_types(e, rhs, type_id)
        }
        ValueKind::Unary { operand, .. } => record_value_tree_types(e, operand, type_id),
        // A `Select`'s arms share its result type; the condition is bool.
        ValueKind::Select { cond, then, else_ } => {
            record_value_tree_types(e, then, type_id)
                && record_value_tree_types(e, else_, type_id)
                && record_value_tree_types(e, cond, crate::tir::TypeTable::BOOL)
        }
        _ => true,
    }
}

/// Freeze re-emittable pure-arith nodes into operand values, function by
/// function. Runs **late** — after every binary-walking pass — so only WIR
/// build (the extractor) ever sees the promoted form; the orphaned arith /
/// local-read skeleton nodes become unreachable from the root and are not
/// emitted.
pub(super) fn freeze_pure_arith(
    project: &mut crate::nir_package::NirPackage,
    include_fields: bool,
    // `early`: this runs before the optimize loop, on each function's
    // freshly-built (clean, un-restructured) graph. Only then is it sound to
    // freeze a *constant leaf read* (`Local` / `FieldAccess`) into its literal:
    // born here, the `Operand::Value` survives `inline` / `sroa` (both copy
    // operands), so no later pass re-contextualizes the read into a position the
    // frozen literal is wrong for. The late freeze (post-loop) must not — its
    // graph carries the loop's structural-edit staleness.
    early: bool,
) -> bool {
    use crate::nir::NirFunction;
    use crate::nir_engine::EngineBuffers;
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for (fid, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        if func.body.is_none() {
            continue;
        }
        // Under build-once persist the freeze is the first (and only) pass to
        // build each function's graph; scope it so the build is attributed here.
        let _vg_scope = super::vg_measure::BuildScope::enter(fid, "freeze");
        let NirFunction {
            body,
            locals,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        // Reassignable locals: a frozen value's `local.get idx` must read the
        // opaque's version, which only holds for single-assignment locals.
        let mut_locals: crate::hashmap::IndexSet<u32> = locals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.is_mut)
            .map(|(i, _)| i as u32)
            .collect();
        let param_set: crate::hashmap::IndexSet<u32> = param_locals.iter().copied().collect();
        // Locals a call may mutate through a `&mut` escape (ref_1's
        // `set_bool(&mut c, …)`). A constant read of one is point-specific and the
        // build-once graph cannot keep it across the structural passes; an
        // *immutable*-`&`-escaped local (licm's `&config`) is stable and its field
        // constant freezes soundly. Keep a copy before `set_alias_sets` moves it.
        let mut_escaped_leaf = mut_escaped.clone();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_param_locals(param_locals);

        // Phase 1: decide every freeze on the clean, unedited graph. A value
        // query never mutates the skeleton, so the verify oracle (which fires
        // on graph queries) only compares build-vs-rebuild here — clean. (A
        // node frozen mid-walk would, inside a loop, leave the maintained
        // graph's recurrence state stale until the next query and read as a
        // spurious over-merge; deciding up front avoids that — and the
        // post-edit graph is not consumed, this being the last pass.)
        let candidates: Vec<ExprId> = engine.body.exprs.keys().collect();
        let mut to_freeze: Vec<(ExprId, ValueId)> = Vec::new();
        for id in candidates {
            if is_assign_target(&engine, id) || is_let_value(&engine, id) {
                continue;
            }
            // Constant-leaf promotion (early only, clean graph). A value-position
            // `Local` / `FieldAccess` read whose graph value is a constant literal
            // is frozen to that literal: context-free, width-correct (the kind
            // carries its own `TypeId`), and — born before any pass — never
            // re-contextualized. `is_place_read` excludes lvalue positions
            // (`&mut x`, a `.field` / method receiver, an assign target) where the
            // storage, not the value, is needed. The root local must not be
            // `mut_escaped`: a write through a retained `&mut` (ref_1's
            // `set_bool(&mut c, …)`) makes the constant point-specific in a way the
            // build-once graph cannot keep across the structural passes (over-merge
            // vs a fresh build). An *immutable*-`&`-escaped root (licm's `&config`)
            // is stable — the callee cannot write through `&Config` — so its field
            // constant freezes soundly; that is the in-loop `FieldAccess` recovery.
            let leaf_root_stable = match &engine.body.exprs[id].kind {
                ExprKind::Local { index, .. } => !mut_escaped_leaf.contains(index),
                ExprKind::FieldAccess { .. } => {
                    super::arena_query::projection_root_local(engine.body, id)
                        .is_none_or(|root| !mut_escaped_leaf.contains(&root))
                }
                _ => false,
            };
            if early
                && leaf_root_stable
                && !is_place_read(&engine, id)
                && let Some(vid) = engine.value(id)
            {
                let rep = engine.value_find(vid);
                if crate::nir_value_graph::builder::is_const_value(&engine.body.values, rep) {
                    to_freeze.push((id, rep));
                    continue;
                }
            }
            if !is_pure_arith(&engine, id, include_fields) {
                continue;
            }
            if let Some(vid) = engine.value(id) {
                let rep = engine.value_find(vid);
                // A standalone `FieldAccess` is reemittable when its receiver is
                // (it materialises via the source-point path). For every other
                // value, `FieldAccess` is non-reemittable (it cannot be inlined),
                // so `value_fully_reemittable_locally` rejects any value nesting
                // one — keeping arith-over-`FieldAccess` from inline promotion.
                let reemittable = match engine.body.values.kind(rep) {
                    ValueKind::FieldAccess { receiver, .. } => {
                        let recv = *receiver;
                        // Conservative placement soundness: only materialise over a
                        // **parameter** receiver. A param is valid (non-null,
                        // available) at every program point, so the hoisted
                        // `let _av = param.field` at the use's enclosing statement
                        // cannot deref a value control flow had not yet validated
                        // (e.g. a loop/`if let` item). Broaden via a dominance check
                        // in P3 to cover local receivers.
                        let recv_src = match engine.body.values.kind(recv) {
                            ValueKind::Opaque(o) => Some(*o),
                            _ => None,
                        }
                        .and_then(|o| engine.body.values.opaque_source(o));
                        let recv_param = matches!(
                            recv_src,
                            Some(crate::nir_value_graph::OpaqueSource::Local(i))
                                if param_set.contains(&i)
                        );
                        recv_param
                            && engine
                                .body
                                .values
                                .value_fully_reemittable_locally(recv, &mut_locals)
                    }
                    _ => engine
                        .body
                        .values
                        .value_fully_reemittable_locally(rep, &mut_locals),
                };
                if reemittable && !engine.body.values.extraction_duplicates_work(rep) {
                    to_freeze.push((id, rep));
                }
            }
        }
        // Phase 2: apply. No further graph queries. Group by representative so a
        // value used by several slots can be **materialised once** (availability
        // extraction, WEP P2) — a single pre-header `let _av = <value>` whose uses
        // read `local.get _av` — instead of re-emitting the computation at each
        // use. Materialisation is gated by `WADO_PROMOTE_EARLY` (keeping the
        // default byte-identical) and limited to values whose leaves are all
        // non-`mut` parameters: those are available and unchanged at function
        // entry, where the `let` is inserted, so it dominates every use soundly.
        // Other values redirect inline as before. `record_value_tree_types` stamps
        // the tree's width and skips a width-conflicting value.
        let promote_early = promote_early_enabled();
        let mut by_rep: crate::hashmap::IndexMap<ValueId, Vec<ExprId>> =
            crate::hashmap::IndexMap::default();
        for (id, rep) in to_freeze {
            by_rep.entry(rep).or_default().push(id);
        }
        for (rep, ids) in by_rep {
            let id_ty = engine.body.exprs[ids[0]].type_id;
            if !record_value_tree_types(&mut engine, rep, id_ty) {
                continue;
            }
            // Materialise a `FieldAccess`: pin the load in a `let _av = <value>`
            // at a statement dominating its uses and rewrite each use to a
            // **skeleton** `Local _av` read, so receiver-position passes see an
            // ordinary local (materialiser-first, WEP P2). A `FieldAccess` is never
            // inline-reemittable (re-emitting a load at an arbitrary slot is
            // unsound once a pass moves the operand), so this is its only promotion
            // path. Single-use places at the use's own statement; multi-use shares
            // one load when all uses lie in one block (see the placement helper).
            // Cross-block uses stay skeleton until real dominance lands.
            let is_field = matches!(engine.body.values.kind(rep), ValueKind::FieldAccess { .. });
            if is_field {
                if let Some((s, b)) = shared_field_materialise_point(&engine, &ids) {
                    let span = engine.body.exprs[ids[0]].span;
                    let name = format!("_av_{}", engine.locals().len());
                    let av = engine.alloc_local(name.clone(), id_ty, /* is_mut */ false);
                    let let_stmt = engine.alloc_stmt(
                        crate::nir_arena::StmtKind::Let {
                            name: name.clone(),
                            local_index: av,
                            is_mut: false,
                            is_reactive: false,
                            type_id: id_ty,
                            value: crate::nir_arena::Operand::Value(rep),
                            skip_value_copy: true,
                        },
                        span,
                    );
                    let mut stmts = engine.body.blocks[b].stmts.clone();
                    let pos = stmts.iter().position(|&x| x == s).unwrap_or(0);
                    stmts.insert(pos, let_stmt);
                    engine.set_block_stmts(b, stmts);
                    for id in ids {
                        let span = engine.body.exprs[id].span;
                        let lread = engine.alloc_expr(
                            ExprKind::Local {
                                index: av,
                                name: name.clone(),
                            },
                            id_ty,
                            span,
                        );
                        changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Expr(lread));
                    }
                }
                continue;
            }
            let mut leaves = crate::hashmap::IndexSet::default();
            engine.body.values.collect_opaque_locals(rep, &mut leaves);
            let materialize = promote_early
                && ids.len() > 1
                && !leaves.is_empty()
                && leaves.iter().all(|l| param_set.contains(l));
            if materialize {
                let name = format!("_av_{}", engine.locals().len());
                let av = engine.alloc_local(name.clone(), id_ty, /* is_mut */ false);
                let read = engine
                    .body
                    .values
                    .fresh_opaque_with_source(crate::nir_value_graph::OpaqueSource::Local(av));
                engine.body.values.set_type(read, id_ty);
                let let_stmt = engine.alloc_stmt(
                    crate::nir_arena::StmtKind::Let {
                        name,
                        local_index: av,
                        is_mut: false,
                        is_reactive: false,
                        type_id: id_ty,
                        value: crate::nir_arena::Operand::Value(rep),
                        skip_value_copy: true,
                    },
                    crate::token::Span::default(),
                );
                let root = engine.body.root;
                let mut stmts = engine.body.blocks[root].stmts.clone();
                stmts.insert(0, let_stmt);
                engine.set_block_stmts(root, stmts);
                for id in ids {
                    changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Value(read));
                }
            } else {
                for id in ids {
                    changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Value(rep));
                }
            }
        }
    }
    changed
}

/// Whether the operand-promotion keystone runs early (mirrors `optimize.rs`); the
/// freeze's materialisation (WEP P2) is gated on it so the default stays inline.
fn promote_early_enabled() -> bool {
    std::env::var_os("WADO_PROMOTE_EARLY").is_some()
}

/// The constant [`Value`] for `rep` if its representative kind is a scalar
/// constant, using `at`'s NIR type for integer width. `None` for a non-constant
/// representative or when folding is disabled (no type table).
pub(super) fn extract_const(
    e: &mut Engine,
    rep: ValueId,
    at: ExprId,
) -> Option<crate::const_eval::Value> {
    if !matches!(
        e.value_kind(rep),
        ValueKind::Int(_, _) | ValueKind::Float(_, _) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return None;
    }
    let vk = e.value_kind(rep).clone();
    let type_id = e.body.exprs[at].type_id;
    let prim = e
        .value_graph_type_table()
        .and_then(|tt| crate::const_eval::prim_of(type_id, tt));
    crate::nir_value_graph::value_kind_to_const(&vk, prim)
}

/// True when `expr` is a **place**, not a value read: the inner of a
/// `Ref` / `MutRef` / `Deref`. A `FieldAccess` here (`&mut obj.field`) addresses
/// the slot, so promoting it to a value would lose the place — exclude it. (The
/// direct `Assign` target is handled by [`is_assign_target`].)
fn is_ref_place(e: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = e.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    matches!(
        &e.body.exprs[parent].kind,
        ExprKind::Unary {
            op: crate::nir::NirUnaryOp::Ref
                | crate::nir::NirUnaryOp::MutRef
                | crate::nir::NirUnaryOp::Deref,
            ..
        }
    )
}

/// True when `expr`'s immediate parent is an `Assign` and `expr` is its target.
fn is_assign_target(e: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = e.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    matches!(
        &e.body.exprs[parent].kind,
        ExprKind::Assign { target, .. } if *target == expr
    )
}

/// Whether `expr` sits in a **place** (lvalue / reference) position, where its
/// storage location — not its value — is what's used: a constant-leaf read here
/// must not be frozen. Covers the `&` / `&mut` / deref referent, a `.field` /
/// `[index]` / method receiver, and an `Assign` LHS. Conservative (a by-value
/// receiver is also rejected), which only forgoes a promotion, never miscompiles.
fn is_place_read(e: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = e.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    use crate::nir::NirUnaryOp;
    let op = crate::nir_arena::Operand::Expr(expr);
    match &e.body.exprs[parent].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => *inner == op,
        ExprKind::Assign { target, .. } => *target == expr,
        ExprKind::FieldAccess { expr: recv, .. } | ExprKind::Index { expr: recv, .. } => {
            *recv == op
        }
        ExprKind::MethodCall { receiver, .. } => *receiver == op,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirLocal};
    use crate::nir_arena::{BlockNode, Body, ExprNode, StmtKind, StmtNode};
    use crate::nir_engine::EngineBuffers;
    use crate::tir::TypeTable;
    use crate::token::Span;

    fn e(body: &mut Body, kind: ExprKind) -> ExprId {
        ei(body, kind, TypeTable::UNIT)
    }

    fn ei(body: &mut Body, kind: ExprKind, type_id: crate::tir::TypeId) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id,
            span: Span::default(),
        })
    }

    #[test]
    fn materializes_a_value_unioned_to_a_literal() {
        use crate::nir_arena::Operand;
        use crate::nir_value_graph::ValueKind;
        // { a + b; 5; } — union the sum's class with the constant 5, then the
        // extractor promotes the sum statement's operand to the pooled `5`.
        let mut body = Body::empty();
        let a = ei(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".into(),
            },
            TypeTable::I32,
        );
        let b = ei(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "b".into(),
            },
            TypeTable::I32,
        );
        let sum = ei(
            &mut body,
            ExprKind::Binary {
                left: a.into(),
                op: NirBinaryOp::Add,
                right: b.into(),
            },
            TypeTable::I32,
        );
        // The constant `5` is a pooled value, born as `Operand::Value`.
        let five_v = body
            .values
            .alloc_unshared(ValueKind::Int(5, TypeTable::I32), TypeTable::I32);
        let s0 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(sum.into()),
            span: Span::default(),
        });
        let s1 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Value(five_v)),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0, s1],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let type_table = TypeTable::default();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        // The extractor reads the operand's primitive from the type table to
        // recover the constant's width.
        eng.set_value_graph_type_table(&type_table);
        let v_sum = eng.value(sum).unwrap();
        // The sum statement still holds the skeleton expression.
        assert!(matches!(
            eng.body.stmts[s0].kind,
            StmtKind::Expr(Operand::Expr(_))
        ));
        // Prove sum ≡ 5 and extract.
        eng.value_union(v_sum, five_v);
        eng.rebuild_value_congruence();
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        eng.run(&rules);
        // The sum statement's operand is now the pooled constant 5.
        let StmtKind::Expr(Operand::Value(v)) = eng.body.stmts[s0].kind else {
            panic!("sum statement operand was not promoted to a value");
        };
        assert!(matches!(eng.body.values.kind(v), ValueKind::Int(5, _)));
    }

    #[test]
    fn leaves_non_constant_values_untouched() {
        // { a + b; } with no union — nothing to materialize.
        let mut body = Body::empty();
        let a = e(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".into(),
            },
        );
        let b = e(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "b".into(),
            },
        );
        let sum = e(
            &mut body,
            ExprKind::Binary {
                left: a.into(),
                op: NirBinaryOp::Add,
                right: b.into(),
            },
        );
        let s0 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(sum.into()),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        let changed = eng.run(&rules);
        assert!(!changed);
        assert!(matches!(eng.body.exprs[sum].kind, ExprKind::Binary { .. }));
    }
}
