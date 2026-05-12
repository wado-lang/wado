//! Shared TIR builders for `i128` / `u128` literal expressions.
//!
//! Lives at the `lower::` top level because it has two callers across
//! the planner / translator boundary:
//!
//! - `lower::plan::globals` — synthesizes default values for lazy-
//!   initialized i128 / u128 globals.
//! - `lower::translate::wide_int` — synthesizes the literal side of
//!   `i128^Eq::eq` / `u128^Eq::eq` calls when rewriting wide-int
//!   `Match` arms into an if-else chain.
//!
//! Both callers must produce identical `Call` shapes because the
//! optimizer and `wir_build` pattern-match on them.

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{CallArg, FunctionRef, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Create an i128 literal expression by calling `i128::from_i64(value)`.
pub(super) fn create_i128_literal(value: i128, type_id: TypeId, span: Span) -> TirExpr {
    let i64_value = value as i64;
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: i64_value.cast_unsigned(),
            repr: value.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info = LocalMethodName::new("i128".to_string(), None, "from_i64".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: "i128::from_i64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
        },
        type_id,
        span,
    )
}

/// Create a u128 literal expression by calling `u128::from_u64(value)`.
pub(super) fn create_u128_literal(value: u128, type_id: TypeId, span: Span) -> TirExpr {
    let u64_value = value as u64;
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: u64_value,
            repr: value.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let method_info = LocalMethodName::new("u128".to_string(), None, "from_u64".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: "u128::from_u64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
        },
        type_id,
        span,
    )
}
