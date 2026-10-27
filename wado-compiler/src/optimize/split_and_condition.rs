//! Split `if a && b { … }` with no `else` into `if a { if b { … } }`.
//!
//! `&&` stays one `Binary` node through NIR, and WIR lowers it to a value,
//! `if (if a { b } else { 0 }) { … }`. Cranelift does not thread a branch on a
//! value built from constants, so the `false` of `a` is materialized and tested
//! again. With no `else` the nested form duplicates nothing. A `b` that is pure
//! and cannot trap is left alone: WIR turns that `&&` into a branchless `a & b`.

use crate::nir::{FuncParts, NirBinaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::token::Span;
use crate::type_table::TypeTable;

use super::arena_query::is_pure_nontrapping_operand_typed;

pub fn split_and_conditions(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let rule = SplitAndConditionRule {
        type_table: &type_table,
    };
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let FuncParts {
            body,
            locals,
            params,
            ..
        } = func.parts();
        if let Some(body) = body {
            let mut engine = Engine::new(body, &mut buffers, locals, params);
            changed |= engine.run(&[&rule]);
        }
    }
    changed
}

struct SplitAndConditionRule<'t> {
    type_table: &'t TypeTable,
}

impl SplitAndConditionRule<'_> {
    /// The operands of `condition` when it is an `a && b` worth branching on.
    fn split(&self, body: &Body, condition: Operand) -> Option<(Operand, Operand)> {
        let ExprKind::Binary {
            op: NirBinaryOp::And,
            left,
            right,
        } = body.exprs[condition.as_expr()?].kind
        else {
            return None;
        };
        (!is_pure_nontrapping_operand_typed(body, right, Some(self.type_table)))
            .then_some((left, right))
    }
}

/// `{ if right { then } }`, the block the outer `if` on `left` takes.
fn inner_if(engine: &mut Engine, right: Operand, then_block: BlockId, span: Span) -> BlockId {
    let inner = engine.alloc_stmt(
        StmtKind::If {
            condition: right,
            then_block,
            else_block: None,
        },
        span,
    );
    engine.alloc_block(vec![inner], span)
}

impl Rule for SplitAndConditionRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::If {
            condition,
            then_branch,
            else_branch: None,
        } = engine.body.exprs[id].kind
        else {
            return false;
        };
        let Some((left, right)) = self.split(engine.body, condition) else {
            return false;
        };
        let span = engine.body.exprs[id].span;
        let then_branch = inner_if(engine, right, then_branch, span);
        engine.replace_expr_kind(
            id,
            ExprKind::If {
                condition: left,
                then_branch,
                else_branch: None,
            },
        );
        true
    }

    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let mut stmts = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in &mut stmts {
            let StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } = engine.body.stmts[*s].kind
            else {
                continue;
            };
            let Some((left, right)) = self.split(engine.body, condition) else {
                continue;
            };
            let span = engine.body.stmts[*s].span;
            let then_block = inner_if(engine, right, then_block, span);
            *s = engine.alloc_stmt(
                StmtKind::If {
                    condition: left,
                    then_block,
                    else_block: None,
                },
                span,
            );
            changed = true;
        }
        if changed {
            engine.set_block_stmts(block, stmts);
        }
        changed
    }
}
