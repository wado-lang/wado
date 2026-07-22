//! Flatten block-tailed `let` bindings — the value-block normal form.
//!
//! Inlining a helper that computes intermediate values before its result leaves
//! the binding wrapped in a block:
//!
//! ```text
//! let x = { let end = v.used; ArraySlice { repr: &v.repr, start: 0, end } }
//! ```
//!
//! Downstream matchers (SROA's candidate collection most notably) key on a
//! direct `let x = <value>`, so the wrapped value is invisible to them. This
//! rule hoists the block's leading statements ahead of the binding and rebinds
//! `x` to the tail value in place:
//!
//! ```text
//! let end = v.used;
//! let x = ArraySlice { repr: &v.repr, start: 0, end }
//! ```
//!
//! Execution order is unchanged (the tail was already last). Applied only when
//! the leading statements are straight-line (`Let` / `Expr` / `LetDestructure`),
//! so no control-flow statement moves across the binding.
//!
//! Edit coherence: the inner block's statement list is cleared *before* the
//! outer splice, so no statement is ever listed in two blocks — a later pop of
//! the orphaned inner block must find it empty, otherwise a rule editing the
//! orphan re-parents live statements into it and corrupts the parent map. The
//! binding itself is rebound via `redirect_expr` (same `StmtId`, same local
//! def), not re-allocated.
//!
//! Together with the fixed-point loop this establishes the normal form: after
//! the post-inline peephole converges, no `let` binds a straight-line
//! value-position `Block`.

use cranelift_entity::EntityRef;

use crate::compiler_trace;
use crate::nir::NirFunction;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;

use super::gate::{FunctionGate, GatedPass};

/// Flatten every block-tailed `let` binding across the package.
///
/// Runs as its own pass between the post-inline peephole and `sroa` — NOT as a
/// rule inside the peephole session. The session's pristine-map rules
/// (`ref_elim`, `elide_box_local`, `labeled_block_fusion`) analyse the body
/// once at session start; a mid-session flatten changes binding shapes under
/// their maps and the rules then interfere (observed: `ref_elim`
/// re-materializing a read of a functor local `elide_box_local` had already
/// elided). As a separate pass, every session sees flattened shapes only in
/// its pristine state.
pub(super) fn flatten_let_blocks(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let rule = LetBlockFlattenRule;
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::LetBlockFlatten, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        let Some(body) = body.as_mut() else {
            return false;
        };
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

pub(super) struct LetBlockFlattenRule;

impl Rule for LetBlockFlattenRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        for (i, &sid) in stmts.iter().enumerate() {
            let Some((block_expr, inner)) = flattenable_inner_block(engine.body, sid) else {
                continue;
            };
            let inner_stmts = engine.body.blocks[inner].stmts.clone();
            let Some((&tail_s, leading)) = inner_stmts.split_last() else {
                continue;
            };
            let StmtKind::Expr(tail_op) = engine.body.stmts[tail_s].kind else {
                continue;
            };
            compiler_trace!(
                "let_block_flatten",
                "flatten let stmt {sid:?}: hoist {} leading stmt(s)",
                leading.len()
            );
            // 1. Strip the orphan-to-be first so no statement is listed twice.
            engine.set_block_stmts(inner, Vec::new());
            // 2. Splice the leading statements ahead of the binding; the
            //    binding statement itself stays (same StmtId, same local def).
            let mut new_stmts = Vec::with_capacity(stmts.len() + leading.len());
            new_stmts.extend_from_slice(&stmts[..i]);
            new_stmts.extend_from_slice(leading);
            new_stmts.extend_from_slice(&stmts[i..]);
            engine.set_block_stmts(block, new_stmts);
            // 3. Rebind the let's value from the block wrapper to the tail.
            //    A skeleton tail MOVES its kind into the wrapper node
            //    (`become_expr`), leaving the orphaned tail statement holding a
            //    `Dead` node — the move-the-kind convention every dropping rule
            //    follows. Redirecting the operand instead would leave the
            //    orphan referencing the live expr, and `Engine::new`'s
            //    arena-wide `build_parents` then lets the orphan overwrite the
            //    live parent edge (observed: copy_prop redirecting through the
            //    stale parent into the orphan, stranding the live read).
            match tail_op {
                crate::nir_arena::Operand::Expr(tail_e) => {
                    engine.become_expr(block_expr, tail_e);
                }
                op @ crate::nir_arena::Operand::Value(_) => {
                    engine.redirect_expr(block_expr, op);
                }
            }
            return true;
        }
        false
    }
}

/// If `sid` is `let x = { … }` whose value is a straight-line block with
/// leading statements and a tail-value `Expr` statement, return the block
/// wrapper expression and the block.
fn flattenable_inner_block(body: &Body, sid: StmtId) -> Option<(ExprId, BlockId)> {
    let StmtKind::Let { value, .. } = &body.stmts[sid].kind else {
        return None;
    };
    let value_e = value.as_expr()?;
    let ExprKind::Block(inner) = &body.exprs[value_e].kind else {
        return None;
    };
    let inner = *inner;
    let (&tail, leading) = body.blocks[inner].stmts.split_last()?;
    if leading.is_empty() {
        return None;
    }
    if !matches!(body.stmts[tail].kind, StmtKind::Expr(_)) {
        return None;
    }
    let straight_line = leading.iter().all(|s| {
        matches!(
            body.stmts[*s].kind,
            StmtKind::Let { .. } | StmtKind::Expr(_) | StmtKind::LetDestructure { .. }
        )
    });
    if !straight_line {
        return None;
    }
    // Defer while a leading statement is a shadow binding the session's
    // dissolvers own: a bare local copy (`let a = b`, the inliner's param
    // binding) or a reference binding (`let r = &place`, `ref_elim`'s domain).
    // Hoisting one first widens its scope and hands the same binding to
    // several session rules at once (observed: a hoisted `let self = get_x`
    // shadow stranding a fabricated `get_x.__capture_0` read after the
    // functor binding was elided). Once the shadows dissolve, the block is
    // single-tail (unwrapped by `const_branch_prune`) or flattenable here on
    // a later iteration.
    let no_shadow_copy = leading.iter().all(|s| {
        let StmtKind::Let { value, .. } = &body.stmts[*s].kind else {
            return true;
        };
        !value.as_expr().is_some_and(|e| {
            matches!(
                body.exprs[e].kind,
                ExprKind::Local { .. }
                    | ExprKind::Unary {
                        op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
                        ..
                    }
            )
        })
    });
    no_shadow_copy.then_some((value_e, inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::hashmap::IndexSet;
    use crate::nir::NirLocal;
    use crate::nir_arena::{BlockNode, ExprNode, Operand, StmtNode};
    use crate::tir::TypeTable;
    use crate::token::Span;

    fn sp() -> Span {
        Span::new(0, 0, 0, 0)
    }

    /// An argument-less call — a straight-line impure prefix value, so the
    /// binding is a genuine intermediate (not a bare-local shadow the gate
    /// defers on).
    fn call_expr() -> ExprKind {
        ExprKind::Call {
            func_id: crate::nir::FuncId::from_u32(0),
            type_args: vec![],
            args: vec![],
        }
    }

    fn local(name: &str) -> NirLocal {
        NirLocal {
            name: name.to_string(),
            type_id: TypeTable::I32,
            is_mut: false,
        }
    }

    fn let_stmt(name: &str, index: u32, value: Operand) -> StmtNode {
        StmtNode {
            kind: StmtKind::Let {
                name: name.to_string(),
                local_index: index,
                is_mut: false,
                is_reactive: false,
                type_id: TypeTable::I32,
                value,
                skip_value_copy: false,
            },
            span: sp(),
        }
    }

    fn expr(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeTable::I32,
            span: sp(),
        })
    }

    /// No statement id may appear in two blocks — the double-listing failure the
    /// clear-inner-first ordering guards against.
    fn assert_single_parent(body: &Body) {
        let mut seen = IndexSet::default();
        for (_, blk) in &body.blocks {
            for &s in &blk.stmts {
                assert!(seen.insert(s), "statement {s:?} appears in two blocks");
            }
        }
    }

    /// `let x = { let p = f(); let y = { let q = f(); (q, q) }; (p, y) }` —
    /// nested value blocks with impure straight-line prefixes. One engine session
    /// must fully un-nest them and keep the single-parent invariant (the nested
    /// case that miscompiled under the old arena hand mutation).
    #[test]
    fn nested_value_blocks_flatten_coherently() {
        let mut body = Body::empty();
        let mut locals = vec![local("p"), local("q"), local("y"), local("x")];

        let r = body.blocks.push(BlockNode {
            stmts: vec![],
            span: sp(),
        });
        let a = body.blocks.push(BlockNode {
            stmts: vec![],
            span: sp(),
        });
        let b = body.blocks.push(BlockNode {
            stmts: vec![],
            span: sp(),
        });
        assert_eq!(r, body.root);

        let arg_q = expr(&mut body, call_expr());
        let q1 = expr(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "q".into(),
            },
        );
        let q2 = expr(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "q".into(),
            },
        );
        let tuple_b = expr(
            &mut body,
            ExprKind::TupleLiteral {
                elements: vec![Operand::Expr(q1), Operand::Expr(q2)],
            },
        );
        let block_b = expr(&mut body, ExprKind::Block(b));
        let arg_p = expr(&mut body, call_expr());
        let p_use = expr(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "p".into(),
            },
        );
        let y_use = expr(
            &mut body,
            ExprKind::Local {
                index: 2,
                name: "y".into(),
            },
        );
        let tuple_a = expr(
            &mut body,
            ExprKind::TupleLiteral {
                elements: vec![Operand::Expr(p_use), Operand::Expr(y_use)],
            },
        );
        let block_a = expr(&mut body, ExprKind::Block(a));

        let s_letq = body.stmts.push(let_stmt("q", 1, Operand::Expr(arg_q)));
        let s_tailb = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(tuple_b)),
            span: sp(),
        });
        let s_letp = body.stmts.push(let_stmt("p", 0, Operand::Expr(arg_p)));
        let s_lety = body.stmts.push(let_stmt("y", 2, Operand::Expr(block_b)));
        let s_taila = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(tuple_a)),
            span: sp(),
        });
        let s_letx = body.stmts.push(let_stmt("x", 3, Operand::Expr(block_a)));

        body.blocks[b].stmts = vec![s_letq, s_tailb];
        body.blocks[a].stmts = vec![s_letp, s_lety, s_taila];
        body.blocks[r].stmts = vec![s_letx];

        let mut buffers = EngineBuffers::default();
        let rule = LetBlockFlattenRule;
        {
            let mut engine = Engine::new(&mut body, &mut buffers, &mut locals);
            engine.run(&[&rule]);
        }

        assert_single_parent(&body);
        assert!(matches!(
            body.exprs[block_a].kind,
            ExprKind::TupleLiteral { .. }
        ));
        assert!(matches!(
            body.exprs[block_b].kind,
            ExprKind::TupleLiteral { .. }
        ));
        let root_lets: Vec<&str> = body.blocks[r]
            .stmts
            .iter()
            .filter_map(|&s| match &body.stmts[s].kind {
                StmtKind::Let { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(root_lets, vec!["p", "q", "y", "x"]);
    }
}
