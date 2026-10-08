//! Flatten a block a statement evaluates first to expose its value to later
//! passes.

use cranelift_entity::EntityRef;

use crate::compiler_trace;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;

use super::arena_query::{block_contains_loop, strip_refs};
use super::gate::{FunctionGate, GatedPass};

/// Flatten every block a statement evaluates first, across the package.
///
/// Its own pass between the post-inline peephole and `sroa`, never a peephole
/// rule: the session's pristine-map rules (`ref_elim`, `elide_box_local`,
/// `labeled_block_fusion`) analyse once at session start, so reshaping bindings
/// mid-session makes them interfere. A separate pass starts every session from
/// flattened shapes.
pub(super) fn flatten_let_blocks(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let rule = LetBlockFlattenRule;
    let functions = &project.functions;
    gate.run_gated_par(GatedPass::LetBlockFlatten, functions.len(), |fid| {
        let mut func = functions[fid.index()].borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        let Some(body) = body.as_mut() else {
            return false;
        };
        let mut buffers = EngineBuffers::default();
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
            // Clear the inner list before splicing, so no statement is listed in
            // two blocks. The binding keeps its `StmtId` and local def.
            engine.set_block_stmts(inner, Vec::new());
            let mut new_stmts = Vec::with_capacity(stmts.len() + leading.len());
            new_stmts.extend_from_slice(&stmts[..i]);
            new_stmts.extend_from_slice(leading);
            new_stmts.extend_from_slice(&stmts[i..]);
            engine.set_block_stmts(block, new_stmts);
            // A skeleton tail moves its kind into the wrapper (`become_expr`),
            // leaving the orphaned tail statement a `Dead` node — never a shared
            // id, which the parent-map tripwire rejects.
            match tail_op {
                Operand::Expr(tail_e) => {
                    engine.become_expr(block_expr, tail_e);
                }
                op @ Operand::Value(_) => {
                    engine.redirect_expr(block_expr, op);
                }
            }
            return true;
        }
        false
    }
}

/// If the operand `sid` evaluates first runs an unbroken block with leading
/// statements and a tail-value `Expr` statement, return the block wrapper
/// expression and the block.
fn flattenable_inner_block(body: &Body, sid: StmtId) -> Option<(ExprId, BlockId)> {
    let bound = strip_refs(body, leading_operand(&body.stmts[sid].kind)?.as_expr()?);
    let value_e = first_evaluated(body, bound);
    // Flattening a block a `break` names would strip the target the jump needs.
    let inner = body.unbroken_block(value_e)?;
    let (&tail, leading) = body.blocks[inner].stmts.split_last()?;
    if leading.is_empty() {
        return None;
    }
    let StmtKind::Expr(tail_value) = body.stmts[tail].kind else {
        return None;
    };
    // Keep a loop available to CTFE: flattening a list-building loop strands
    // its evaluation outside the value region (`ctfe_list_result`). A bound
    // block gives that up where its tail exposes a literal for SROA. An
    // argument never does, since it is the callee that reads the value
    // (`ctfe_struct_result`).
    let literal_tail = value_e == bound
        && tail_value.as_expr().is_some_and(|e| {
            matches!(
                body.exprs[e].kind,
                ExprKind::StructLiteral { .. } | ExprKind::TupleLiteral { .. }
            )
        });
    if !literal_tail && block_contains_loop(body, inner) {
        return None;
    }
    // Defer while a leading statement is a shadow a session dissolver owns: a
    // immutable local copy (`let a = b`, the inliner's param binding, copy_prop's) or
    // a reference to a *place* (`let r = &x`, ref_elim's). Hoisting one hands the
    // same binding to several rules at once (observed: a hoisted `let self =
    // get_x` stranded a `get_x.$capture_0` read after the functor was elided);
    // once dissolved, the block flattens on a later iteration. A reference to a
    // *fresh value* (`&String { … }`, `&f()`) is nobody's shadow — deferring
    // would strand it block-wrapped forever — so the reference case gates on
    // place referents only.
    let no_shadow_copy = leading.iter().all(|s| {
        let StmtKind::Let { value, is_mut, .. } = &body.stmts[*s].kind else {
            return true;
        };
        !value.as_expr().is_some_and(|e| match &body.exprs[e].kind {
            ExprKind::Local { .. } => !is_mut,
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr: referent,
            } => referent
                .as_expr()
                .is_some_and(|re| is_place_expr(&body.exprs[re].kind)),
            _ => false,
        })
    });
    no_shadow_copy.then_some((value_e, inner))
}

/// The operand a statement evaluates before it does anything else. A loop's
/// body runs again, so nothing in it is first.
fn leading_operand(kind: &StmtKind) -> Option<Operand> {
    match kind {
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Expr(value)
        | StmtKind::If {
            condition: value, ..
        } => Some(*value),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => *value,
        StmtKind::Loop { .. } | StmtKind::Continue | StmtKind::LabeledBlock { .. } => None,
    }
}

/// The subexpression of `e` that runs first, which is where a block's leading
/// statements can hoist to the binding's place without reordering anything.
///
/// A reference around it applies to the tail's value either way:
/// `&mut { …; Struct { … } }` is what an inlined by-reference receiver leaves.
/// A call's argument is first when every argument ahead of it is a constant:
/// `f({ …; Slice { … } })` is what an inlined constructor leaves in argument
/// position, and hoisting it hands the literal to the split parameter ABI.
fn first_evaluated(body: &Body, e: ExprId) -> ExprId {
    let ExprKind::Call { args, .. } = &body.exprs[e].kind else {
        return e;
    };
    for arg in args {
        match arg.expr {
            Operand::Expr(first) => return first_evaluated(body, strip_refs(body, first)),
            Operand::Value(v) if body.values.kind(v).is_constant() => {}
            // A promoted read of a local a leading statement may assign.
            Operand::Value(_) => return e,
        }
    }
    e
}

/// A place expression — the lvalue forms `ref_elim` dissolves a `&`/`&mut` of.
fn is_place_expr(kind: &ExprKind) -> bool {
    matches!(
        kind,
        ExprKind::Local { .. } | ExprKind::FieldAccess { .. } | ExprKind::Index { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_args::CallArgs;
    use std::assert_matches;

    use crate::hashmap::IndexSet;
    use crate::nir::{FuncId, NirLocal};
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
            func_id: FuncId::from_u32(0),
            type_args: vec![],
            args: CallArgs::free(vec![]),
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
        let block_b = expr(&mut body, ExprKind::plain_block(b, TypeTable::UNIT, "test"));
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
        let block_a = expr(&mut body, ExprKind::plain_block(a, TypeTable::UNIT, "test"));

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
        assert_matches!(body.exprs[block_a].kind, ExprKind::TupleLiteral { .. });
        assert_matches!(body.exprs[block_b].kind, ExprKind::TupleLiteral { .. });
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
