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
//!
//! Values that fit `i64` / `u64` are emitted as
//! `i128::from_i64(v)` / `u128::from_u64(v)`. Values outside that
//! range are emitted as `i128::from_pair(lo, hi)` /
//! `u128::from_pair(lo, hi)` so the full 128 bits round-trip — same
//! split the resolver uses for source-level literals (see
//! `resolver::util::unpack_i128`, `resolver::call::build_from_pair_call`).

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{CallArg, FunctionRef, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Create an i128 literal TIR expression that evaluates to `value`.
pub(super) fn create_i128_literal(value: i128, type_id: TypeId, span: Span) -> TirExpr {
    if let Ok(fits) = i64::try_from(value) {
        return build_i128_from_i64_call(fits, value, type_id, span);
    }
    let (low, high) = (value as u64, (value >> 64) as i64);
    build_i128_from_pair_call(low, high, type_id, span)
}

/// Create a u128 literal TIR expression that evaluates to `value`.
pub(super) fn create_u128_literal(value: u128, type_id: TypeId, span: Span) -> TirExpr {
    if let Ok(fits) = u64::try_from(value) {
        return build_u128_from_u64_call(fits, value, type_id, span);
    }
    let (low, high) = (value as u64, (value >> 64) as u64);
    build_u128_from_pair_call(low, high, type_id, span)
}

fn build_i128_from_i64_call(value: i64, original: i128, type_id: TypeId, span: Span) -> TirExpr {
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: value.cast_unsigned(),
            repr: original.to_string(),
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

fn build_u128_from_u64_call(value: u64, original: u128, type_id: TypeId, span: Span) -> TirExpr {
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value,
            repr: original.to_string(),
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

fn build_i128_from_pair_call(low: u64, high: i64, type_id: TypeId, span: Span) -> TirExpr {
    let low_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: low,
            repr: low.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let high_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: high.cast_unsigned(),
            repr: high.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info = LocalMethodName::new("i128".to_string(), None, "from_pair".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
        },
        type_id,
        span,
    )
}

fn build_u128_from_pair_call(low: u64, high: u64, type_id: TypeId, span: Span) -> TirExpr {
    let low_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: low,
            repr: low.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let high_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: high,
            repr: high.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let method_info = LocalMethodName::new("u128".to_string(), None, "from_pair".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
        },
        type_id,
        span,
    )
}
