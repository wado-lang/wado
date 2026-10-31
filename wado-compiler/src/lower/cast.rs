//! A source `as` cast as TIR: the references it reads through, the prelude
//! calls a half precision float or a wide integer needs, then the bare `Cast`
//! codegen takes as one instruction or none.
//!
//! A matter of representation rather than a recorded decision: a function of
//! the type table alone, which reify calls for every cast.

use std::cell::RefCell;
use std::ops::ControlFlow;

use crate::compiler_item::CompilerItem;
use crate::lower::wide_int_literal::{create_conversion, method_ref};
use crate::primitive::PrimitiveType;
use crate::synthesis::common::{bare_cast, handle_from_f64, handle_to_f64, rt_call, unary_call};
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirUnaryOp, TypeId, to_layers_of};
use crate::token::Span;
use crate::type_table::TypeTable;

/// `operand as target`.
pub(crate) fn lower_cast(
    type_table: &RefCell<TypeTable>,
    operand: TirExpr,
    target: TypeId,
    span: Span,
) -> TirExpr {
    let read = read_through(&type_table.borrow(), operand, target);
    cast_read(type_table, read, target, span)
}

/// The exact `f32` that `value`, of the half type `half`, holds.
pub(crate) fn widen_half(
    type_table: &TypeTable,
    value: TirExpr,
    half: PrimitiveType,
    span: Span,
) -> TirExpr {
    let (base, widen) = match half {
        PrimitiveType::F16 => (TypeTable::F16, CompilerItem::F16Widen),
        PrimitiveType::Bf16 => (TypeTable::BF16, CompilerItem::Bf16Widen),
        _ => unreachable!("`{}` is no half", half.as_str()),
    };
    rt_call(
        type_table,
        widen,
        bare_cast(value, base, span),
        TypeTable::F32,
        span,
    )
}

/// `operand` dereferenced through what a cast to `target` reads through.
fn read_through(type_table: &TypeTable, operand: TirExpr, target: TypeId) -> TirExpr {
    type_table
        .cast_read_through(operand.type_id, target)
        .into_iter()
        .fold(operand, |operand, referent| {
            let span = operand.span;
            TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(operand),
                },
                referent,
                span,
            )
        })
}

/// `inner as target`, `inner` already read through its references.
fn cast_read(
    type_table: &RefCell<TypeTable>,
    inner: TirExpr,
    target: TypeId,
    span: Span,
) -> TirExpr {
    if is_newtype_step(&type_table.borrow(), inner.type_id, target) {
        return bare_cast(inner, target, span);
    }
    let inner = match half_cast(type_table, inner, target, span) {
        ControlFlow::Break(lowered) => return lowered,
        ControlFlow::Continue(inner) => inner,
    };
    let inner = match wide_int_cast(type_table, inner, target, span) {
        ControlFlow::Break(lowered) => return lowered,
        ControlFlow::Continue(inner) => inner,
    };
    let (from_handle, to_handle) = {
        let tt = type_table.borrow();
        (
            tt.is_unrestricted_handle(inner.type_id),
            tt.is_unrestricted_handle(target),
        )
    };
    let inner = match (from_handle, to_handle) {
        (true, false) => handle_to_f64(inner),
        (false, true) => return handle_from_f64(inner, target),
        _ => inner,
    };
    bare_cast(inner, target, span)
}

/// Whether `source as target` changes no representation: both share one, or
/// either is still generic, where the checker admits only a newtype step
/// (`cast_needs_no_judgment`).
fn is_newtype_step(type_table: &TypeTable, source: TypeId, target: TypeId) -> bool {
    type_table.share_common_base(source, target)
        || type_table.representation_awaits_substitution(source)
        || type_table.representation_awaits_substitution(target)
}

/// A cast with `f16` or `bf16` on either side. No instruction converts either,
/// so a half widens to its exact `f32` and casts on from there, and a number
/// reaches a half through the prelude conversion that rounds it once.
/// `Continue` hands `inner` on where neither side is a half.
fn half_cast(
    type_table: &RefCell<TypeTable>,
    inner: TirExpr,
    target: TypeId,
    span: Span,
) -> ControlFlow<TirExpr, TirExpr> {
    let (source_half, target_half) = {
        let tt = type_table.borrow();
        let half = |id| tt.primitive_head(id).filter(|p| p.is_half());
        (half(inner.type_id), half(target))
    };
    if source_half.is_none() && target_half.is_none() {
        return ControlFlow::Continue(inner);
    }
    let source = match source_half {
        Some(half) => widen_half(&type_table.borrow(), inner, half, span),
        None => inner,
    };
    let Some(half) = target_half else {
        return ControlFlow::Break(cast_read(type_table, source, target, span));
    };
    let (base, item, via) = {
        let tt = type_table.borrow();
        let source_base = tt.representation_head(source.type_id);
        match (
            half,
            tt.primitive_head(source_base),
            tt.wide_int_item(source_base),
        ) {
            (PrimitiveType::F16, _, _) => {
                (TypeTable::F16, CompilerItem::F16FromF64, TypeTable::F64)
            }
            (_, _, Some(CompilerItem::I128)) => {
                (TypeTable::BF16, CompilerItem::Bf16FromI128, source_base)
            }
            (_, _, Some(_)) => (TypeTable::BF16, CompilerItem::Bf16FromU128, source_base),
            (_, Some(PrimitiveType::I64), _) => {
                (TypeTable::BF16, CompilerItem::Bf16FromI64, TypeTable::I64)
            }
            (_, Some(PrimitiveType::U64), _) => {
                (TypeTable::BF16, CompilerItem::Bf16FromU64, TypeTable::U64)
            }
            _ => (TypeTable::BF16, CompilerItem::Bf16FromF64, TypeTable::F64),
        }
    };
    let arg = cast_read(type_table, source, via, span);
    let converted = rt_call(&type_table.borrow(), item, arg, base, span);
    ControlFlow::Break(bare_cast(converted, target, span))
}

/// A cast with a wide integer on either side. The 128-bit types are prelude
/// structs, so a bare `Cast` would leak the boxed struct ref into a slot
/// expecting a Wasm scalar (issue #1328), or a scalar into one expecting the
/// struct. `Continue` hands `inner` on to the bare cast where neither side
/// needs a lowering.
fn wide_int_cast(
    type_table: &RefCell<TypeTable>,
    inner: TirExpr,
    target: TypeId,
    span: Span,
) -> ControlFlow<TirExpr, TirExpr> {
    let tt = type_table.borrow();
    if tt.is_wide_int(inner.type_id) {
        drop(tt);
        return wide_int_source_cast(type_table, inner, target, span);
    }
    let Some(item) = tt.wide_int_item(target) else {
        return ControlFlow::Continue(inner);
    };
    ControlFlow::Break(create_conversion(item, inner, target, &tt, span))
}

/// `i128/u128 as T`: `f64`/`f32` through the correctly rounded
/// `as_f64`/`as_f32`, integer targets through `low()` plus a primitive cast
/// (truncation), and `i128 ↔ u128` through the bit-reinterpreting
/// `from_u128`/`from_i128` constructors. Any other target hands `inner` on;
/// `resolve_cast` has already reported it if it is invalid.
fn wide_int_source_cast(
    type_table: &RefCell<TypeTable>,
    inner: TirExpr,
    target: TypeId,
    span: Span,
) -> ControlFlow<TirExpr, TirExpr> {
    enum Lowering {
        /// `&self` accessor returning the target primitive directly.
        Method(CompilerItem),
        /// `low()` then a primitive cast down to the target width.
        LowThenCast,
        /// Static bit-reinterpreting constructor of the other wide type.
        Reinterpret(CompilerItem),
    }

    // Newtypes share their base's representation, so dispatch on the ultimate
    // base of both sides; explicit repr-compatible `Cast` nodes bridge the
    // newtype boundaries below.
    let (source_base, target_base, source_item, target_item, target_resolved) = {
        let tt = type_table.borrow();
        let source_base = tt.representation_head(inner.type_id);
        let target_base = tt.representation_head(target);
        (
            source_base,
            target_base,
            tt.wide_int_item(source_base)
                .expect("the caller checked the source is a wide integer"),
            tt.wide_int_item(target_base),
            tt.get(target_base).clone(),
        )
    };
    let signed_source = source_item == CompilerItem::I128;

    let lowering = match target_resolved {
        ResolvedType::Primitive(PrimitiveType::F64) => Lowering::Method(if signed_source {
            CompilerItem::I128AsF64
        } else {
            CompilerItem::U128AsF64
        }),
        ResolvedType::Primitive(PrimitiveType::F32) => Lowering::Method(if signed_source {
            CompilerItem::I128AsF32
        } else {
            CompilerItem::U128AsF32
        }),
        ResolvedType::Primitive(
            PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::I16
            | PrimitiveType::U16
            | PrimitiveType::I8
            | PrimitiveType::U8,
        ) => Lowering::LowThenCast,
        _ => match target_item {
            Some(item) => {
                assert_ne!(
                    item, source_item,
                    "`cast_read` takes a cast within one base as a newtype step"
                );
                Lowering::Reinterpret(if signed_source {
                    CompilerItem::U128FromI128
                } else {
                    CompilerItem::I128FromU128
                })
            }
            None => return ControlFlow::Continue(inner),
        },
    };

    // A newtype source first steps to its wide base so the prelude calls below
    // see their declared receiver/argument type.
    let inner = bare_cast(inner, source_base, span);
    let accessor = |item, inner, result_type| {
        let func = method_ref(&type_table.borrow(), item);
        let ref_type = type_table
            .borrow_mut()
            .intern(ResolvedType::Ref(source_base));
        let receiver = to_layers_of(inner, ref_type, &type_table.borrow());
        TirExpr::new(
            TirExprKind::method_call(Box::new(receiver), func, vec![], vec![]),
            result_type,
            span,
        )
    };
    let converted = match lowering {
        Lowering::Method(item) => accessor(item, inner, target_base),
        Lowering::LowThenCast => {
            let item = if signed_source {
                CompilerItem::I128Low
            } else {
                CompilerItem::U128Low
            };
            bare_cast(accessor(item, inner, TypeTable::U64), target_base, span)
        }
        Lowering::Reinterpret(item) => {
            let func = method_ref(&type_table.borrow(), item);
            unary_call(func, inner, target_base, span)
        }
    };
    ControlFlow::Break(bare_cast(converted, target, span))
}
