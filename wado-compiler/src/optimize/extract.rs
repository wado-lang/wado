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
/// freeze candidate.
fn is_pure_arith(e: &Engine, id: ExprId) -> bool {
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
    )
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
        _ => true,
    }
}

/// Freeze re-emittable pure-arith nodes into operand values, function by
/// function. Runs **late** — after every binary-walking pass — so only WIR
/// build (the extractor) ever sees the promoted form; the orphaned arith /
/// local-read skeleton nodes become unreachable from the root and are not
/// emitted.
pub(super) fn freeze_pure_arith(project: &mut crate::nir_package::NirPackage) -> bool {
    use crate::nir::NirFunction;
    use crate::nir_engine::EngineBuffers;
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.body.is_none() {
            continue;
        }
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
            if is_assign_target(&engine, id)
                || is_let_value(&engine, id)
                || !is_pure_arith(&engine, id)
            {
                continue;
            }
            if let Some(vid) = engine.value(id) {
                let rep = engine.value_find(vid);
                if engine
                    .body
                    .values
                    .value_fully_reemittable_locally(rep, &mut_locals)
                    && !engine.body.values.extraction_duplicates_work(rep)
                {
                    to_freeze.push((id, rep));
                }
            }
        }
        // Phase 2: apply. No further graph queries. Stamp `rep`'s tree from the
        // frozen node's type; skip the freeze on a width conflict (a value
        // shared with a differently-typed use), which would extract at the
        // wrong width.
        for (id, rep) in to_freeze {
            let id_ty = engine.body.exprs[id].type_id;
            if record_value_tree_types(&mut engine, rep, id_ty) {
                changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Value(rep));
            }
        }
    }
    changed
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
        ValueKind::Int(_) | ValueKind::Float(_) | ValueKind::Bool(_) | ValueKind::Char(_)
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
        let five_v = body.values.alloc_unshared(ValueKind::Int(5), TypeTable::I32);
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
        assert!(matches!(eng.body.values.kind(v), ValueKind::Int(5)));
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
