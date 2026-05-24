//! Select lowering optimization for Wado NIR
//!
//! Post-optimization rewrite that converts simple `if cond { a } else { b }` expressions
//! to `builtin::select(cond, a, b)`, which emits the branchless Wasm `select` instruction.
//! Both branches must be pure (no side effects, no traps) since `select` evaluates both
//! operands eagerly.

use std::cell::RefCell;
use std::rc::Rc;

use crate::module_source::ModuleSource;
use crate::nir::{
    CallArg, FunctionRef, MonomorphInfo, NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

use crate::nir_visitor::{NirOptVisitor, opt_walk_expr};

/// Run select lowering on all functions.
pub fn select_lowering(project: &mut NirPackage) {
    let mut visitor = SelectLoweringVisitor {
        type_table: Rc::clone(&project.type_table),
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            visitor.visit_block(body);
        }
    }
}

struct SelectLoweringVisitor {
    type_table: Rc<RefCell<TypeTable>>,
}

impl NirOptVisitor for SelectLoweringVisitor {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        let mut changed = false;
        if let NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &mut expr.kind
        {
            // Scope the TypeTable borrow tightly so the recursive
            // `self.visit_expr` call below can re-borrow `self`.
            let select_call = {
                let type_table = self.type_table.borrow();
                try_lower_to_select(
                    condition,
                    then_branch,
                    else_branch,
                    expr.type_id,
                    expr.span,
                    &type_table,
                )
            };
            if let Some(select_call) = select_call {
                *expr = select_call;
                changed = true;
                changed |= self.visit_expr(expr);
                return changed;
            }
        }
        changed |= opt_walk_expr(self, expr);
        changed
    }
}

/// True when `expr` is eligible to appear as a `builtin::select` arm.
///
/// `select` evaluates both arms unconditionally, so each arm must be:
///
/// - **Side-effect free** — no `Call` / `Assign` / mutation that an
///   omitted branch evaluation would have skipped.
/// - **Non-trapping** — no `Div` / `Mod` (integer divide-by-zero
///   traps), no `Deref` (null-deref traps), no float→int `Cast`
///   (Wasm `i*.trunc_f*_s/u` traps on NaN / infinity / out of range).
///   Floating-point arithmetic never traps in Wasm, so float `Div`
///   would actually be safe — but the integer-shape rule excludes it
///   uniformly because we don't peek at operand types here.
/// - **Cheap to compute redundantly** — the speculation cost on the
///   not-taken branch should be small. Restrict the shape to
///   duplicable leaves (`Local`, literals) plus a single layer of
///   pure leaf operators (`Unary` over a leaf-pure operand, `Binary`
///   over two leaf-pure operands, `Cast` of a leaf-pure value whose
///   source/target combination cannot trap). The recursion bottoms
///   out at leaves; nested operator trees over leaf-pure
///   subexpressions are allowed but in practice rarely reach beyond
///   depth 2-3 from source.
///
/// The Wado runtime never traps on `Ref` / `MutRef` (taking a local's
/// address is harmless), but reference-taking inside a `select` arm
/// is a code-smell shape that we keep out of scope.
fn is_select_eligible_expr(expr: &NirExpr, type_table: &TypeTable) -> bool {
    match &expr.kind {
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Local { .. } => true,
        NirExprKind::Unary { op, expr: inner } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && is_select_eligible_expr(inner, type_table)
        }
        NirExprKind::Binary { op, left, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_select_eligible_expr(left, type_table)
                && is_select_eligible_expr(right, type_table)
        }
        NirExprKind::Cast {
            expr: inner,
            target_type,
        } => {
            !is_trapping_cast(inner.type_id, *target_type, type_table)
                && is_select_eligible_expr(inner, type_table)
        }
        _ => false,
    }
}

/// True when an `as` cast from `src` to `dst` lowers to a Wasm
/// instruction that traps at runtime.
///
/// The trapping shapes are exactly `f32`/`f64` → integer: WIR build
/// emits `i32.trunc_f64_s` / `i64.trunc_f32_u` and friends
/// (`wir_build/primitive_ops.rs::translate_cast`), all of which trap on
/// NaN, infinity, or values outside the target integer's range. Every
/// other Wado cast lowers to a wrap / extend / convert / promote /
/// demote instruction that has no trap semantics, or to an
/// identity-at-WIR pass-through (struct casts).
fn is_trapping_cast(src: TypeId, dst: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        (type_table.get(src), type_table.get(dst)),
        (
            ResolvedType::Primitive(PrimitiveType::F32 | PrimitiveType::F64),
            ResolvedType::Primitive(
                PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::I16
                    | PrimitiveType::U16
                    | PrimitiveType::I32
                    | PrimitiveType::U32
                    | PrimitiveType::I64
                    | PrimitiveType::U64
            ),
        )
    )
}

fn try_select_value<'a>(block: &'a NirBlock, type_table: &TypeTable) -> Option<&'a NirExpr> {
    if block.stmts.len() != 1 {
        return None;
    }
    if let NirStmtKind::Expr(expr) = &block.stmts[0].kind
        && is_select_eligible_expr(expr, type_table)
    {
        return Some(expr);
    }
    None
}

fn try_lower_to_select(
    condition: &NirExpr,
    then_branch: &NirBlock,
    else_branch: &Option<NirBlock>,
    result_type: TypeId,
    span: crate::token::Span,
    type_table: &TypeTable,
) -> Option<NirExpr> {
    let else_block = else_branch.as_ref()?;

    if result_type == TypeTable::UNIT {
        return None;
    }

    let true_val = try_select_value(then_branch, type_table)?;
    let false_val = try_select_value(else_block, type_table)?;

    let func_ref = FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "select".to_string(),
        monomorph_info: Some(MonomorphInfo {
            generic_name: "select".to_string(),
            impl_type_args: vec![result_type],
            method_type_args: vec![],
            is_blanket: false,
        }),
        method_info: None,
    };

    Some(NirExpr::new(
        NirExprKind::Call {
            func: func_ref,
            type_args: vec![result_type],
            args: vec![
                CallArg::new(condition.clone(), false),
                CallArg::new(true_val.clone(), false),
                CallArg::new(false_val.clone(), false),
            ],
        },
        result_type,
        span,
    ))
}
