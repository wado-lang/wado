//! Select lowering optimization for Wado NIR.
//!
//! Post-optimization rewrite that converts simple `if cond { a } else { b }`
//! expressions to `builtin::select(cond, a, b)`, which emits the branchless
//! Wasm `select` instruction. Both branches must be pure (no side effects, no
//! traps) since `select` evaluates both operands eagerly.
//!
//! This is the first peephole pass ported to the worklist rewrite engine
//! (Phase 4 stage C; see `docs/wep-2026-06-05-nir-rewrite-engine-design.md`):
//! it runs as a [`Rule`] over each function's arena `Body` directly, with no
//! `Body ↔ tree` bridge. The `select` Call reuses the existing condition / arm
//! expression ids, so the rewrite is a single `replace_expr_kind` with no node
//! allocation. The rule is confluent — a `select` arm must be leaf-pure, so an
//! arm can never itself be an `If` / `Call`, and the worklist's bottom-up order
//! produces the same result the old top-down visitor did.

use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, MonomorphInfo, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprId, ExprKind, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

/// Run select lowering on all functions, driven by the rewrite engine.
pub fn select_lowering(project: &mut NirPackage) {
    let type_table = project.type_table.borrow();
    let rule = SelectLoweringRule {
        type_table: &type_table,
    };
    let mut buffers = EngineBuffers::default();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let mut engine = Engine::new(body, &mut buffers);
            engine.run(&[&rule]);
        }
    }
}

struct SelectLoweringRule<'t> {
    type_table: &'t TypeTable,
}

impl Rule for SelectLoweringRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &engine.body.exprs[id].kind
        else {
            return false;
        };
        let condition = *condition;
        let then_branch = *then_branch;
        let Some(else_branch) = *else_branch else {
            return false;
        };
        let result_type = engine.body.exprs[id].type_id;
        if result_type == TypeTable::UNIT {
            return false;
        }
        let Some(true_val) = arm_select_value(engine.body, then_branch, self.type_table) else {
            return false;
        };
        let Some(false_val) = arm_select_value(engine.body, else_branch, self.type_table) else {
            return false;
        };

        let func = FunctionRef {
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
        engine.replace_expr_kind(
            id,
            ExprKind::Call {
                func,
                type_args: vec![result_type],
                args: vec![
                    ArenaCallArg {
                        expr: condition,
                        is_mut: false,
                    },
                    ArenaCallArg {
                        expr: true_val,
                        is_mut: false,
                    },
                    ArenaCallArg {
                        expr: false_val,
                        is_mut: false,
                    },
                ],
            },
        );
        true
    }
}

/// A branch is select-able when it is a single `Expr` statement whose value is
/// select-eligible; returns that value's id.
fn arm_select_value(body: &Body, block: BlockId, type_table: &TypeTable) -> Option<ExprId> {
    let stmts = &body.blocks[block].stmts;
    if stmts.len() != 1 {
        return None;
    }
    if let StmtKind::Expr(e) = &body.stmts[stmts[0]].kind {
        let e = *e;
        if is_select_eligible(body, e, type_table) {
            return Some(e);
        }
    }
    None
}

/// True when `id` is eligible to appear as a `builtin::select` arm: a
/// duplicable leaf (`Local`, literal) or a single layer of pure leaf
/// operators over leaf-pure operands, none of which traps. See the original
/// pass doc for the full rationale.
fn is_select_eligible(body: &Body, id: ExprId, type_table: &TypeTable) -> bool {
    match &body.exprs[id].kind {
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::Local { .. } => true,
        ExprKind::Unary { op, expr: inner } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && is_select_eligible(body, *inner, type_table)
        }
        ExprKind::Binary { op, left, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_select_eligible(body, *left, type_table)
                && is_select_eligible(body, *right, type_table)
        }
        ExprKind::Cast {
            expr: inner,
            target_type,
        } => {
            !is_trapping_cast(body.exprs[*inner].type_id, *target_type, type_table)
                && is_select_eligible(body, *inner, type_table)
        }
        _ => false,
    }
}

/// True when an `as` cast from `src` to `dst` lowers to a trapping Wasm
/// instruction (only `f32`/`f64` → integer traps; everything else is a
/// wrap / extend / convert / identity).
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
