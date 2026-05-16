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
//!
//! The struct, module source, and method names are all resolved
//! through the `CompilerItem` registry so a stdlib rename of `i128`,
//! `u128`, or any of their `from_*` constructors stays transparent.

use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{CallArg, FunctionRef, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Snapshot of a wide-int constructor's registry coordinates, taken
/// before we lock the type table for builders that mutate it.
struct CtorRef {
    module_source: ModuleSource,
    type_name: String,
    method_name: String,
}

fn ctor_ref(type_table: &TypeTable, owner: CompilerItem, ctor: CompilerItem) -> CtorRef {
    let items = type_table.compiler_items();
    let (owner_module, type_name) = items.require_struct(owner);
    let (method_module, _, method_name) = items.require_method(ctor);
    // Production stdlib places i128 / u128 and their constructors in
    // the same module; assert that invariant so a future split is
    // diagnosed rather than silently producing mismatched `Call.module_source`
    // versus `method_info.struct_name` pairs.
    debug_assert_eq!(owner_module, method_module);
    CtorRef {
        module_source: owner_module.clone(),
        type_name: type_name.to_string(),
        method_name: method_name.to_string(),
    }
}

/// Create an i128 literal TIR expression that evaluates to `value`.
pub(super) fn create_i128_literal(
    value: i128,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    if let Ok(fits) = i64::try_from(value) {
        let ctor = ctor_ref(type_table, CompilerItem::I128, CompilerItem::I128FromI64);
        return build_i128_from_i64_call(fits, value, type_id, &ctor, span);
    }
    let (low, high) = (value as u64, (value >> 64) as i64);
    let ctor = ctor_ref(type_table, CompilerItem::I128, CompilerItem::I128FromPair);
    build_i128_from_pair_call(low, high, type_id, &ctor, span)
}

/// Create a u128 literal TIR expression that evaluates to `value`.
pub(super) fn create_u128_literal(
    value: u128,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    if let Ok(fits) = u64::try_from(value) {
        let ctor = ctor_ref(type_table, CompilerItem::U128, CompilerItem::U128FromU64);
        return build_u128_from_u64_call(fits, value, type_id, &ctor, span);
    }
    let (low, high) = (value as u64, (value >> 64) as u64);
    let ctor = ctor_ref(type_table, CompilerItem::U128, CompilerItem::U128FromPair);
    build_u128_from_pair_call(low, high, type_id, &ctor, span)
}

fn build_i128_from_i64_call(
    value: i64,
    original: i128,
    type_id: TypeId,
    ctor: &CtorRef,
    span: Span,
) -> TirExpr {
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: value.cast_unsigned(),
            repr: original.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info =
        LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    let mangled_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ctor.module_source.clone(),
                name: mangled_name,
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

fn build_u128_from_u64_call(
    value: u64,
    original: u128,
    type_id: TypeId,
    ctor: &CtorRef,
    span: Span,
) -> TirExpr {
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value,
            repr: original.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let method_info =
        LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    let mangled_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ctor.module_source.clone(),
                name: mangled_name,
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

fn build_i128_from_pair_call(
    low: u64,
    high: i64,
    type_id: TypeId,
    ctor: &CtorRef,
    span: Span,
) -> TirExpr {
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
    let method_info =
        LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ctor.module_source.clone(),
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

fn build_u128_from_pair_call(
    low: u64,
    high: u64,
    type_id: TypeId,
    ctor: &CtorRef,
    span: Span,
) -> TirExpr {
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
    let method_info =
        LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ctor.module_source.clone(),
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
