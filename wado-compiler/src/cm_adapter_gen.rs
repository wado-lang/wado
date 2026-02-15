//! CM Adapter Synthesis phase.
//!
//! Generates TIR adapter functions for Component Model boundary crossing.
//! Each adapter handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures adapter functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-adapter-synthesis.md` for design details.

use indexmap::IndexSet;

use crate::ast::Type;
use crate::cm_abi;
use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId, TypeTable};
use crate::token::Span;

/// Synthetic span used for all generated adapter code.
fn synth_span() -> Span {
    Span::new(0, 0, 1, 1)
}

// ============================================================================
// TIR construction helpers
// ============================================================================

/// Create a call to a builtin function (e.g., `builtin::i32_load`).
pub fn builtin_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("builtin"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create a call to an internal function (e.g., `internal::cm_lower_string`).
pub fn internal_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("internal"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create an i32 literal expression.
pub fn i32_const(value: i32) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I32,
        synth_span(),
    )
}

/// Create an i64 literal expression.
pub fn i64_const(value: i64) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I64,
        synth_span(),
    )
}

/// Create a local variable reference.
pub fn local_ref(index: u32, name: &str, type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: name.to_string(),
        },
        type_id,
        synth_span(),
    )
}

/// Create a binary expression.
pub fn binary(op: crate::tir::TirBinaryOp, left: TirExpr, right: TirExpr, ty: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        synth_span(),
    )
}

/// Create a cast expression.
pub fn cast(expr: TirExpr, target_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(expr),
            target_type,
        },
        target_type,
        synth_span(),
    )
}

/// Create a let statement.
pub fn let_stmt(name: &str, local_index: u32, type_id: TypeId, value: TirExpr) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index,
            is_mut: false,
            is_reactive: false,
            type_id,
            value,
        },
        synth_span(),
    )
}

/// Create an expression statement.
pub fn expr_stmt(expr: TirExpr) -> TirStmt {
    TirStmt::new(TirStmtKind::Expr(expr), synth_span())
}

/// Create a return statement.
pub fn return_stmt(value: Option<TirExpr>) -> TirStmt {
    TirStmt::new(TirStmtKind::Return { value }, synth_span())
}

/// Create a `CmRawCall` expression targeting a lowered WASI import.
pub fn cm_raw_call(local_name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::CmRawCall {
            local_name: local_name.to_string(),
            args,
        },
        return_type,
        synth_span(),
    )
}

// ============================================================================
// Lift / Lower synthesis
// ============================================================================

/// Synthesize a TIR expression that loads a CM value from linear memory.
///
/// For primitives, this is a single `builtin::i32_load` (or similar).
/// For String, this reads (ptr, len) and calls `memory_to_gc_string`.
pub fn synthesize_lift(ty: &Type, addr: TirExpr) -> TirExpr {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => builtin_call("i32_load", vec![addr], TypeTable::I32),
            "i64" | "u64" => builtin_call("i64_load", vec![addr], TypeTable::I64),
            "f32" => builtin_call("f32_load", vec![addr], TypeTable::F32),
            "f64" => builtin_call("f64_load", vec![addr], TypeTable::F64),
            "i8" | "u8" => builtin_call("i32_load8_u", vec![addr], TypeTable::I32),
            "i16" | "u16" => builtin_call("i32_load16_u", vec![addr], TypeTable::I32),
            "bool" => {
                let raw = builtin_call("i32_load8_u", vec![addr], TypeTable::I32);
                binary(
                    crate::tir::TirBinaryOp::NotEq,
                    raw,
                    i32_const(0),
                    TypeTable::BOOL,
                )
            }
            "char" => builtin_call("i32_load", vec![addr], TypeTable::CHAR),
            "String" => {
                let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
                let len = builtin_call(
                    "i32_load",
                    vec![binary(
                        crate::tir::TirBinaryOp::Add,
                        addr,
                        i32_const(4),
                        TypeTable::I32,
                    )],
                    TypeTable::I32,
                );
                // Return type uses I32 as placeholder; caller must fix up
                // to the actual String TypeId from the type table.
                internal_call("memory_to_gc_string", vec![ptr, len], TypeTable::I32)
            }
            _ => panic!("synthesize_lift: unsupported type `{}`", named.name),
        },
        Type::Tuple(elems) if elems.is_empty() => {
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span())
        }
        _ => panic!("synthesize_lift: unsupported type {ty:?}"),
    }
}

/// Synthesize TIR statements that store a Wado value into linear memory.
///
/// For primitives, this is a single `builtin::i32_store` (or similar).
pub fn synthesize_lower(ty: &Type, value: TirExpr, addr: TirExpr) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i64" | "u64" => vec![expr_stmt(builtin_call(
                "i64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f32" => vec![expr_stmt(builtin_call(
                "f32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f64" => vec![expr_stmt(builtin_call(
                "f64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i8" | "u8" => vec![expr_stmt(builtin_call(
                "i32_store8",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i16" | "u16" => vec![expr_stmt(builtin_call(
                "i32_store16",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "bool" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store8",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "char" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            _ => panic!("synthesize_lower: unsupported type `{}`", named.name),
        },
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => panic!("synthesize_lower: unsupported type {ty:?}"),
    }
}

// ============================================================================
// Flat ABI parameter/result computation
// ============================================================================

/// Compute the flat ABI parameter types for a WASI function parameter.
pub fn flatten_param_type(ty: &Type) -> Vec<TypeId> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" | "bool" | "char" | "i8" | "u8" | "i16" | "u16" => {
                vec![TypeTable::I32]
            }
            "i64" | "u64" => vec![TypeTable::I64],
            "f32" => vec![TypeTable::F32],
            "f64" => vec![TypeTable::F64],
            "String" => vec![TypeTable::I32, TypeTable::I32],
            _ => vec![TypeTable::I32],
        },
        Type::Generic(g) if g.name == "Stream" => vec![TypeTable::I32],
        Type::Reference(_) | Type::MutReference(_) => vec![TypeTable::I32],
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => {
            let flat = cm_abi::cm_flat_types(ty);
            flat.iter()
                .map(|t| match t {
                    cm_abi::CmValType::I32 => TypeTable::I32,
                    cm_abi::CmValType::I64 => TypeTable::I64,
                    cm_abi::CmValType::F32 => TypeTable::F32,
                    cm_abi::CmValType::F64 => TypeTable::F64,
                })
                .collect()
        }
    }
}

// ============================================================================
// Adapter function generation
// ============================================================================

/// Adapter function name prefix.
const ADAPTER_PREFIX: &str = "__cm_adapter__";

/// Build the adapter function name for a WASI import.
pub fn adapter_func_name(effect_name: &str, method_name: &str) -> String {
    format!("{ADAPTER_PREFIX}{effect_name}_{method_name}")
}

/// Phase entry point: generate CM adapter functions.
///
/// Currently a no-op scaffolding that enumerates effect calls without
/// transforming them. As WASI interfaces are migrated, this will:
/// 1. Synthesize adapter TIR functions for each used WASI import
/// 2. Rewrite `Call` nodes targeting effect modules to call the adapters
pub fn generate_adapters(project: Project) -> Project {
    let mut _seen_effects: IndexSet<String> = IndexSet::new();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut _seen_effects);
            }
        }
    }
    project
}

fn collect_effect_calls_in_block(block: &TirBlock, effects: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        collect_effect_calls_in_stmt(stmt, effects);
    }
}

fn collect_effect_calls_in_stmt(stmt: &TirStmt, effects: &mut IndexSet<String>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_effect_calls_in_expr(value, effects);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_effect_calls_in_expr(condition, effects);
            collect_effect_calls_in_block(then_block, effects);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_effect_calls_in_block(body, effects);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            collect_effect_calls_in_block(then_block, effects);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects);
            }
        }
        TirStmtKind::Continue | TirStmtKind::LetPattern { .. } => {}
    }
}

fn collect_effect_calls_in_expr(expr: &TirExpr, effects: &mut IndexSet<String>) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let module_source = func.module_source();
            if module_source.is_effect_like()
                && let Some(effect_name) = module_source.effect_name()
            {
                let method_name = func.name();
                effects.insert(format!("{effect_name}::{method_name}"));
            }
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::EffectCall { args, .. }
        | TirExprKind::CmRawCall { args, .. }
        | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_effect_calls_in_expr(receiver, effects);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_effect_calls_in_expr(callee, effects);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_effect_calls_in_block(block, effects);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_effect_calls_in_expr(left, effects);
            collect_effect_calls_in_expr(right, effects);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { expr: inner } => {
            collect_effect_calls_in_expr(inner, effects);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_effect_calls_in_expr(condition, effects);
            collect_effect_calls_in_block(then_branch, effects);
            if let Some(blk) = else_branch {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            collect_effect_calls_in_expr(e, effects);
            collect_effect_calls_in_expr(index, effects);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            for arm in arms {
                collect_effect_calls_in_block(arm, effects);
            }
            collect_effect_calls_in_block(default, effects);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_effect_calls_in_expr(guard, effects);
                }
                collect_effect_calls_in_expr(&arm.body, effects);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_effect_calls_in_expr(body, effects);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_effect_calls_in_expr(functor, effects);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_effect_calls_in_expr(&field.value, effects);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_effect_calls_in_expr(value, effects);
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        // Catch-all for any remaining leaf or rare variants
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NamedType;

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            name: name.to_string(),
            span: Span::new(0, 0, 1, 1),
        })
    }

    #[test]
    fn flatten_param_i32() {
        assert_eq!(flatten_param_type(&named_type("i32")), vec![TypeTable::I32]);
    }

    #[test]
    fn flatten_param_i64() {
        assert_eq!(flatten_param_type(&named_type("i64")), vec![TypeTable::I64]);
    }

    #[test]
    fn flatten_param_f64() {
        assert_eq!(flatten_param_type(&named_type("f64")), vec![TypeTable::F64]);
    }

    #[test]
    fn flatten_param_string() {
        assert_eq!(
            flatten_param_type(&named_type("String")),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        assert_eq!(
            flatten_param_type(&named_type("bool")),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        assert!(flatten_param_type(&Type::Tuple(vec![])).is_empty());
    }

    #[test]
    fn adapter_name() {
        assert_eq!(
            adapter_func_name("Stdout", "write_via_stream"),
            "__cm_adapter__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let expr = synthesize_lift(&named_type("i32"), i32_const(100));
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_bool() {
        let expr = synthesize_lift(&named_type("bool"), i32_const(100));
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_string() {
        let expr = synthesize_lift(&named_type("String"), i32_const(100));
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lower_i32() {
        let stmts = synthesize_lower(&named_type("i32"), i32_const(42), i32_const(100));
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(&named_type("bool"), value, i32_const(100));
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(&Type::Tuple(vec![]), value, i32_const(100));
        assert!(stmts.is_empty());
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }
}
