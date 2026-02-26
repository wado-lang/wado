//! Constant propagation optimization for Wado TIR
//!
//! This module propagates compile-time-known global variable values into their
//! use sites. When a global variable is:
//! - Not mutable (`mutable == false` after lowering)
//! - Initialized with a scalar constant (int, float, bool, char literal)
//!
//! all `GlobalVarGet` references to that global are replaced with the constant value.
//!
//! Note: After the lower phase, immutable globals with non-constant initializers
//! have already been converted to `mutable == true` for lazy initialization.
//! Therefore, any global with `mutable == false` at optimization time is guaranteed
//! to have a constant initializer.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind};
use indexmap::IndexMap;

/// A constant value extracted from a global variable's initializer.
#[derive(Debug, Clone)]
enum ConstValue {
    Int { value: u64, repr: String },
    Float { value: f64, repr: String },
    Bool(bool),
    Char(char),
}

impl ConstValue {
    fn to_expr_kind(&self) -> TirExprKind {
        match self {
            Self::Int { value, repr } => TirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Float { value, repr } => TirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Bool(b) => TirExprKind::BoolLiteral(*b),
            Self::Char(c) => TirExprKind::CharLiteral(*c),
        }
    }
}

/// Try to extract a scalar constant from a global initializer expression.
fn extract_const_value(expr: &TirExpr) -> Option<ConstValue> {
    match &expr.kind {
        TirExprKind::IntLiteral { value, repr } => Some(ConstValue::Int {
            value: *value,
            repr: repr.clone(),
        }),
        TirExprKind::FloatLiteral { value, repr } => Some(ConstValue::Float {
            value: *value,
            repr: repr.clone(),
        }),
        TirExprKind::BoolLiteral(b) => Some(ConstValue::Bool(*b)),
        TirExprKind::CharLiteral(c) => Some(ConstValue::Char(*c)),
        _ => None,
    }
}

/// Key for looking up a global variable by its module source and name.
type GlobalKey = (ModuleSource, String);

/// Collect all constant globals from the project.
/// Returns a map from (`module_source`, name) to the constant value.
fn collect_constant_globals(project: &Project) -> IndexMap<GlobalKey, ConstValue> {
    let mut constants: IndexMap<GlobalKey, ConstValue> = IndexMap::new();

    for module in project.tir_modules.values() {
        for global in &module.globals {
            // Only propagate non-mutable globals (after lowering, mutable == false
            // means the global truly has a constant initializer)
            if global.mutable {
                continue;
            }

            if let Some(value) = extract_const_value(&global.initializer) {
                constants.insert((global.module_source.clone(), global.name.clone()), value);
            }
        }
    }

    constants
}

/// Apply constant propagation to all functions in the project.
pub fn propagate_constants(project: &mut Project) -> bool {
    let constants = collect_constant_globals(project);
    if constants.is_empty() {
        return false;
    }

    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= propagate_constants_in_function(&mut func, &constants);
        }
    }
    changed
}

fn propagate_constants_in_function(
    func: &mut TirFunction,
    constants: &IndexMap<GlobalKey, ConstValue>,
) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    propagate_constants_in_block(body, constants)
}

fn propagate_constants_in_block(
    block: &mut TirBlock,
    constants: &IndexMap<GlobalKey, ConstValue>,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= propagate_constants_in_stmt(stmt, constants);
    }
    changed
}

fn propagate_constants_in_stmt(
    stmt: &mut TirStmt,
    constants: &IndexMap<GlobalKey, ConstValue>,
) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => propagate_constants_in_expr(value, constants),
        TirStmtKind::Expr(expr) => propagate_constants_in_expr(expr, constants),
        TirStmtKind::Return { value } => value
            .as_mut()
            .is_some_and(|v| propagate_constants_in_expr(v, constants)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = propagate_constants_in_expr(condition, constants);
            changed |= propagate_constants_in_block(then_block, constants);
            if let Some(eb) = else_block {
                changed |= propagate_constants_in_block(eb, constants);
            }
            changed
        }
        TirStmtKind::Loop { body } => propagate_constants_in_block(body, constants),
        TirStmtKind::LabeledBlock { block, .. } => propagate_constants_in_block(block, constants),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = propagate_constants_in_expr(scrutinee, constants);
            changed |= propagate_constants_in_block(then_block, constants);
            if let Some(eb) = else_block {
                changed |= propagate_constants_in_block(eb, constants);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => value
            .as_mut()
            .is_some_and(|v| propagate_constants_in_expr(v, constants)),
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => propagate_constants_in_expr(value, constants),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn propagate_constants_in_expr(
    expr: &mut TirExpr,
    constants: &IndexMap<GlobalKey, ConstValue>,
) -> bool {
    // Check if this is a GlobalVarGet that can be replaced with a constant
    if let TirExprKind::GlobalVarGet {
        module_source,
        name,
    } = &expr.kind
    {
        let key = (module_source.clone(), name.clone());
        if let Some(const_value) = constants.get(&key) {
            expr.kind = const_value.to_expr_kind();
            return true;
        }
    }

    // Recurse into sub-expressions
    let mut changed = false;
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= propagate_constants_in_expr(left, constants);
            changed |= propagate_constants_in_expr(right, constants);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            changed |= propagate_constants_in_expr(inner, constants);
        }
        TirExprKind::Assign { target, value } => {
            changed |= propagate_constants_in_expr(target, constants);
            changed |= propagate_constants_in_expr(value, constants);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= propagate_constants_in_expr(arg, constants);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= propagate_constants_in_expr(receiver, constants);
            for arg in args {
                changed |= propagate_constants_in_expr(arg, constants);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= propagate_constants_in_expr(callee, constants);
            for arg in args {
                changed |= propagate_constants_in_expr(arg, constants);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= propagate_constants_in_expr(functor, constants);
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            changed |= propagate_constants_in_expr(inner, constants);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= propagate_constants_in_expr(inner, constants);
            changed |= propagate_constants_in_expr(index, constants);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= propagate_constants_in_block(block, constants);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= propagate_constants_in_expr(condition, constants);
            changed |= propagate_constants_in_block(then_branch, constants);
            if let Some(eb) = else_branch {
                changed |= propagate_constants_in_block(eb, constants);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= propagate_constants_in_expr(inner, constants);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= propagate_constants_in_expr(guard, constants);
                }
                changed |= propagate_constants_in_expr(&mut arm.body, constants);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= propagate_constants_in_expr(&mut field.value, constants);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= propagate_constants_in_expr(elem, constants);
            }
        }
        TirExprKind::OptionSome { value } => {
            changed |= propagate_constants_in_expr(value, constants);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                changed |= propagate_constants_in_expr(payload_expr, constants);
            }
        }
        TirExprKind::Move { expr } => {
            changed |= propagate_constants_in_expr(expr, constants);
        }
        TirExprKind::Closure { body, .. } => {
            changed |= propagate_constants_in_expr(body, constants);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= propagate_constants_in_expr(value, constants);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            changed |= propagate_constants_in_expr(expr, constants);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= propagate_constants_in_expr(scrutinee, constants);
            for arm in arms {
                changed |= propagate_constants_in_block(arm, constants);
            }
            changed |= propagate_constants_in_block(default, constants);
        }
        // Leaf nodes - nothing to recurse into
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
    }

    changed
}
