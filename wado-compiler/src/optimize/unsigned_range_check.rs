//! A range check against a non-negative bound is one unsigned comparison:
//! `0 <= x && x < n` is `(x as u32) < (n as u32)`, since a negative `x` reads
//! as at least `2^31`, past every `n` below it. The violation `x < 0 || x >= n`
//! is likewise `(n as u32) <= (x as u32)`. Every `List` index contract is the
//! first shape, so each index pays one compare where it paid two and an `and`.
//!
//! It runs after the bounds-check eliminators, which read the two halves of the
//! signed pair separately and would lose the lower half under a cast.

use crate::nir::{FuncParts, NirBinaryOp};
use crate::nir_arena::{ExprId, ExprKind, Operand};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::primitive::PrimitiveType;
use crate::tir::TypeId;
use crate::type_table::TypeTable;

use super::arena_query::{binary_parts, is_pure_nontrapping_operand_typed};
use super::condition_implication::{
    Binds, build_copy_bindings, is_seq_length, operand_same, parse_const_i64,
};

pub fn fuse_unsigned_range_checks(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let FuncParts { body, locals, .. } = func.parts();
        if let Some(body) = body {
            let mut engine = Engine::new(body, &mut buffers, locals);
            engine.set_value_graph_type_table(&type_table);
            // A snapshot, as condition implication's: a fused check writes no
            // local, so no bind it holds goes stale.
            let rule = UnsignedRangeCheckRule {
                types: &type_table,
                binds: build_copy_bindings(&engine),
            };
            changed |= engine.run(&[&rule]);
        }
    }
    changed
}

struct UnsignedRangeCheckRule<'t> {
    types: &'t TypeTable,
    binds: Binds,
}

/// Which way a fused check answers.
#[derive(Clone, Copy)]
enum Shape {
    /// `0 <= x && x < n` (or `<= n`): holds inside the range.
    Inside,
    /// `x < 0 || x >= n` (or `> n`): holds outside it.
    Outside,
}

impl UnsignedRangeCheckRule<'_> {
    /// `a op b` with `op` turned to `<` or `<=` by swapping the sides of a `>`
    /// or `>=`. Only a comparison written in place: the fused one takes over
    /// its operands, which a `let`'s value still holds.
    fn ordered(&self, engine: &Engine, op: Operand) -> Option<(Operand, NirBinaryOp, Operand)> {
        let (left, kind, right) = binary_parts(engine.body, op)?;
        match kind {
            NirBinaryOp::Lt | NirBinaryOp::LtEq => Some((left, kind, right)),
            NirBinaryOp::Gt | NirBinaryOp::GtEq => Some((right, kind.converse()?, left)),
            _ => None,
        }
    }

    /// The `x` of `0 <= x` (`Inside`) or `x < 0` (`Outside`).
    fn zero_side(&self, engine: &Engine, op: Operand, shape: Shape) -> Option<Operand> {
        let (left, kind, right) = self.ordered(engine, op)?;
        let konst = |o| parse_const_i64(engine, &self.binds, o);
        let holds = match (shape, kind) {
            (Shape::Inside, NirBinaryOp::LtEq) => konst(left) == Some(0),
            (Shape::Inside, NirBinaryOp::Lt) => konst(left) == Some(-1),
            (Shape::Outside, NirBinaryOp::Lt) => konst(right) == Some(0),
            (Shape::Outside, NirBinaryOp::LtEq) => konst(right) == Some(-1),
            _ => false,
        };
        holds.then_some(match shape {
            Shape::Inside => right,
            Shape::Outside => left,
        })
    }

    /// The `(x, op, n)` of `x < n` / `x <= n` (`Inside`), or the `(n, op, x)` of
    /// `x > n` / `x >= n` (`Outside`) read as `n < x` / `n <= x`, where `n` is
    /// known non-negative.
    fn bound_side(
        &self,
        engine: &Engine,
        op: Operand,
        x: Operand,
        shape: Shape,
    ) -> Option<(Operand, NirBinaryOp, Operand)> {
        let (left, kind, right) = self.ordered(engine, op)?;
        let (index, bound) = match shape {
            Shape::Inside => (left, right),
            Shape::Outside => (right, left),
        };
        (operand_same(engine, &self.binds, index, x) && self.non_negative(engine, bound))
            .then_some((left, kind, right))
    }

    fn non_negative(&self, engine: &Engine, op: Operand) -> bool {
        is_seq_length(engine, &self.binds, op)
            || parse_const_i64(engine, &self.binds, op).is_some_and(|c| c >= 0)
    }

    /// The unsigned type of the same width as the signed `ty`.
    fn unsigned_of(&self, ty: TypeId) -> Option<TypeId> {
        match self.types.primitive_head(ty)? {
            PrimitiveType::I32 => Some(TypeTable::U32),
            PrimitiveType::I64 => Some(TypeTable::U64),
            _ => None,
        }
    }

    /// The comparison `id` fuses into: its halves, each in either order.
    fn fused(&self, engine: &Engine, id: ExprId) -> Option<(Operand, NirBinaryOp, Operand)> {
        let ExprKind::Binary { left, op, right } = engine.body.exprs[id].kind else {
            return None;
        };
        let shape = match op {
            NirBinaryOp::And | NirBinaryOp::BitAnd => Shape::Inside,
            NirBinaryOp::Or | NirBinaryOp::BitOr => Shape::Outside,
            _ => return None,
        };
        if engine.body.exprs[id].type_id != TypeTable::BOOL
            || !is_pure_nontrapping_operand_typed(engine.body, right, Some(self.types))
        {
            return None;
        }
        [(left, right), (right, left)]
            .into_iter()
            .find_map(|(zero, bound)| {
                let x = self.zero_side(engine, zero, shape)?;
                self.bound_side(engine, bound, x, shape)
            })
    }
}

impl Rule for UnsignedRangeCheckRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let Some((left, op, right)) = self.fused(engine, id) else {
            return false;
        };
        let Some(unsigned) = self.unsigned_of(engine.body.operand_type(left)) else {
            return false;
        };
        let span = engine.body.exprs[id].span;
        let cast = |engine: &mut Engine, expr| {
            Operand::Expr(engine.alloc_expr(
                ExprKind::Cast {
                    expr,
                    target_type: unsigned,
                },
                unsigned,
                span,
            ))
        };
        let (left, right) = (cast(engine, left), cast(engine, right));
        engine.replace_expr_kind(id, ExprKind::Binary { left, op, right });
        true
    }
}
