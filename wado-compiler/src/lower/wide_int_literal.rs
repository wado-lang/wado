//! Shared TIR builders for `i128` / `u128` literals, at the `lower::` top level
//! for its two callers either side of the planner / translator boundary, which
//! must produce identical `Call` shapes for the optimizer and `wir_build` to
//! match on. A value fitting 64 bits emits `from_i64` / `from_u64`, anything
//! wider `from_pair(lo, hi)` — the elaborator's own split for source literals.

use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName};
use crate::tir::{CallArg, FunctionRef, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Snapshot of a wide-int constructor's registry coordinates, taken
/// before we lock the type table for builders that mutate it.
struct CtorRef {
    module_source: ModuleSource,
    type_name: FqTypeName,
    method_name: String,
}

fn ctor_ref(type_table: &TypeTable, owner: CompilerItem, ctor: CompilerItem) -> CtorRef {
    let type_name = type_table.compiler_struct_fq_name(owner);
    let items = type_table.compiler_items();
    let (owner_module, _) = items.require_struct(owner);
    let (method_module, _, method_name) = items.require_method(ctor);
    // Production stdlib places i128 / u128 and their constructors in
    // the same module; assert that invariant so a future split is
    // diagnosed rather than silently producing mismatched `Call.module_source`
    // versus `method_info.struct_name` pairs.
    debug_assert_eq!(owner_module, method_module);
    CtorRef {
        module_source: owner_module.clone(),
        type_name,
        method_name: method_name.to_string(),
    }
}

/// Create an i128 literal TIR expression that evaluates to `value`.
pub(crate) fn create_i128_literal(
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
pub(crate) fn create_u128_literal(
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
    let method_info = LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    let mangled_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ctor.module_source.clone(),
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
            has_receiver: false,
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
    let method_info = LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    let mangled_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ctor.module_source.clone(),
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
            has_receiver: false,
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
    let method_info = LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ctor.module_source.clone(),
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
            has_receiver: false,
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
    let method_info = LocalMethodName::new(ctor.type_name.clone(), None, ctor.method_name.clone());
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ctor.module_source.clone(),
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
            has_receiver: false,
        },
        type_id,
        span,
    )
}
