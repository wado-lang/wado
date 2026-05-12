//! Shared TIR helpers for building `i128` / `u128` literal expressions.
//!
//! Used by both the global-initializer lowering (`lower/globals.rs`)
//! and the wide-int match → if-else translator
//! (`lower/translate/wide_int.rs`). The two callers must agree on how
//! the literal is represented because the optimizer / `wir_build`
//! pattern-match on the resulting `Call` shape.

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
