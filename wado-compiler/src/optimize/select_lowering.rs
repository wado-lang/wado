//! Select lowering: a post-optimization [`Rule`] turning `if cond { a } else
//! { b }` into `builtin::select(cond, a, b)` and its branchless Wasm
//! instruction. Both arms must be pure and trap-free, `select` evaluating them
//! eagerly. The rewrite reuses the existing expression ids, so it is one
//! `replace_expr_kind`, and leaf-purity makes the rule confluent.

use crate::lower::plan::value_copy::needs_value_copy;
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprId, ExprKind, Operand, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

/// Run select lowering on all functions, driven by the rewrite engine.
pub fn select_lowering(project: &mut NirPackage) -> bool {
    let select_id = intern_select(project);
    let type_table = project.type_table.borrow();
    let rule = SelectLoweringRule {
        type_table: &type_table,
        select_id,
    };
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        if let Some(body) = body.as_mut() {
            let mut engine = Engine::new(body, &mut buffers, locals);
            changed |= engine.run(&[&rule]);
        }
    }
    changed
}

/// Intern the `select` builtin once so every synthesized call is born
/// resolved. Its `FuncId` keys on `Free(builtin, "select")` (type args ride
/// the node), so one id serves all instantiations.
pub(super) fn intern_select(project: &mut NirPackage) -> crate::nir::FuncId {
    project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "select".to_string(),
        monomorph_info: None,
        method_info: None,
    })
}

struct SelectLoweringRule<'t> {
    type_table: &'t TypeTable,
    select_id: crate::nir::FuncId,
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
        // A `select` returns one of its operands. For a deep-copied result that
        // aliases the chosen arm's live storage, so lower only scalar results;
        // the normal `if` lowering copies the arm.
        if needs_value_copy(result_type, self.type_table) {
            return false;
        }
        let Some(true_val) = arm_select_value(engine.body, then_branch, self.type_table) else {
            return false;
        };
        let Some(false_val) = arm_select_value(engine.body, else_branch, self.type_table) else {
            return false;
        };

        engine.replace_expr_kind(
            id,
            select_call(self.select_id, result_type, condition, true_val, false_val),
        );
        true
    }
}

/// `builtin::select(cond, a, b)` over `ty`.
pub(super) fn select_call(
    select_id: crate::nir::FuncId,
    ty: TypeId,
    cond: Operand,
    a: Operand,
    b: Operand,
) -> ExprKind {
    let arg = |expr| ArenaCallArg {
        expr,
        is_mut: false,
    };
    ExprKind::Call {
        func_id: select_id,
        type_args: vec![ty],
        args: vec![arg(cond), arg(a), arg(b)],
        has_receiver: false,
    }
}

/// A branch is select-able when it is a single `Expr` statement whose value is
/// select-eligible; returns that value as an operand. A skeleton tail
/// (`Operand::Expr`) is checked structurally; a born-as-operand scalar leaf
/// (`Operand::Value`, e.g. the `1` / `2` of `if c { 1 } else { 2 }`, or a bare
/// local read) is accepted directly — it is a pure, non-trapping, duplicable
/// leaf that the value pool already holds.
fn arm_select_value(body: &Body, block: BlockId, type_table: &TypeTable) -> Option<Operand> {
    let stmts = &body.blocks[block].stmts;
    if stmts.len() != 1 {
        return None;
    }
    match &body.stmts[stmts[0]].kind {
        StmtKind::Expr(Operand::Expr(e)) => {
            let e = *e;
            is_select_eligible(body, e, type_table).then_some(Operand::Expr(e))
        }
        StmtKind::Expr(Operand::Value(v)) => {
            let v = *v;
            is_select_eligible_value(body, v, type_table).then_some(Operand::Value(v))
        }
        _ => None,
    }
}

/// A promoted pure value is select-eligible under the same rule as a skeleton
/// arm ([`is_select_eligible`]): a duplicable leaf — a scalar constant or local
/// read — or pure non-trapping operators over such leaves. The two must stay in
/// step, an arm reaching one or the other only by whether promotion froze it. A
/// [`ValueKind::Const`] aggregate stays out; `select` takes scalars anyway.
fn is_select_eligible_value(body: &Body, v: ValueId, type_table: &TypeTable) -> bool {
    let kind = body.values.kind(v);
    if kind.is_operand_constant() {
        return true;
    }
    match kind {
        ValueKind::Opaque(oid) => {
            matches!(
                body.values.opaque_source(*oid),
                Some(OpaqueSource::Local(_))
            )
        }
        ValueKind::Unary { op, operand, .. } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && is_select_eligible_value(body, *operand, type_table)
        }
        ValueKind::Binary { op, lhs, rhs, .. } => {
            !super::arena_query::binary_op_may_trap(*op)
                && is_select_eligible_value(body, *lhs, type_table)
                && is_select_eligible_value(body, *rhs, type_table)
        }
        // `value_fully_reemittable_locally` refuses a value nesting a `Cast`,
        // for want of the operand's source type — the type a trap test would
        // need here too. Asserted, not refused: were the freeze to admit one,
        // a `false` would hide the missing trap test as a lost lowering.
        ValueKind::Cast { .. } => unreachable!(
            "select-arm eligibility reached a promoted `Cast`; the freeze decision refuses one"
        ),
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::Null
        | ValueKind::Unit
        | ValueKind::Const(..)
        | ValueKind::Select { .. }
        | ValueKind::LoopPhi { .. }
        | ValueKind::FieldAccess { .. } => false,
    }
}

/// True when `op` is eligible inside a `builtin::select` arm, whichever form it
/// takes. Both must be asked: refusing a promoted operand loses every lowering
/// whose operand happens to be frozen, and admitting one unasked speculates a
/// trapping `a / b`, since `select` evaluates both arms.
fn is_select_eligible_operand(body: &Body, op: Operand, type_table: &TypeTable) -> bool {
    match op {
        Operand::Expr(e) => is_select_eligible(body, e, type_table),
        Operand::Value(v) => is_select_eligible_value(body, v, type_table),
    }
}

fn is_select_eligible(body: &Body, id: ExprId, type_table: &TypeTable) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Unary { op, expr: inner } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && is_select_eligible_operand(body, *inner, type_table)
        }
        ExprKind::Binary { op, left, right } => {
            // Both operands are duplicated into the `select`; a trapping op
            // (`Div` / `Mod`) must not be lowered, since a `select` evaluates
            // both arms unconditionally. The trap taxonomy is shared with
            // `arena_query` so it cannot drift from the other trap consumers.
            !super::arena_query::binary_op_may_trap(*op)
                && is_select_eligible_operand(body, *left, type_table)
                && is_select_eligible_operand(body, *right, type_table)
        }
        ExprKind::Cast {
            expr: inner,
            target_type,
        } => {
            !is_trapping_cast(body.operand_type(*inner), *target_type, type_table)
                && is_select_eligible_operand(body, *inner, type_table)
        }
        _ => false,
    }
}

/// True when an `as` cast from `src` to `dst` lowers to a trapping Wasm
/// instruction — only float → integer; everything else wraps, extends,
/// converts, or is the identity. Deliberately finer than the shared
/// `arena_query::expr_node_may_trap`, which marks every `Cast` trap-capable, and
/// kept local so the other consumers' classification is unchanged.
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
