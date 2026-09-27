//! TIR builders for `i128` / `u128` values — both are prelude structs, so a
//! literal is a constructor call and a comparison an `Eq` / `Ord` call.
//!
//! At the `lower::` top level for callers either side of the planner /
//! translator boundary: the optimizer and `wir_build` match on the `Call` shape,
//! so every producer must emit the same one.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::elaborator::reify::ord_bool_from_cmp;
use crate::name::LocalMethodName;
use crate::synthesis::common::not_expr;
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

/// The wide-int method `method` as a callee, named by the registry so that no
/// producer spells it.
pub(crate) fn method_ref(type_table: &TypeTable, method: CompilerItem) -> FunctionRef {
    let items = type_table.compiler_items();
    let (module_source, _, name) = items.require_method(method);
    let method_info = LocalMethodName::new(
        items.require_method_owner(method).clone(),
        None,
        name.to_string(),
    );
    FunctionRef {
        module_source: module_source.clone(),
        name: method_info.to_mangled_name(),
        template: None,
        monomorph_info: None,
        method_info: Some(method_info),
    }
}

/// A call of the static wide-int constructor `ctor` on `args`.
fn ctor_call(
    ctor: CompilerItem,
    args: Vec<TirExpr>,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(method_ref(type_table, ctor)),
            type_args: vec![],
            args: args.into_iter().map(|a| CallArg::new(a, false)).collect(),
            has_receiver: false,
        },
        type_id,
        span,
    )
}

/// Which wide-int constructor a call names, and how its arguments compose into
/// the 128-bit pattern. [`classify_ctor`] recognises every call
/// [`create_literal`] or [`create_conversion`] emits from an integer, so a
/// consumer reading a wide-int constant back cannot drift from the producers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WideIntCtor {
    /// `<i128|u128>::from_i64(v)` — sign-extends.
    FromI64,
    /// `<i128|u128>::from_u64(v)` — zero-extends.
    FromU64,
    /// `<i128|u128>::from_pair(low, high)` — the halves verbatim.
    FromPair,
}

impl WideIntCtor {
    /// Every `(owner, shape)` pair a wide-int constant can take.
    const ALL: [(CompilerItem, Self); 6] = [
        (CompilerItem::I128, Self::FromI64),
        (CompilerItem::I128, Self::FromU64),
        (CompilerItem::I128, Self::FromPair),
        (CompilerItem::U128, Self::FromU64),
        (CompilerItem::U128, Self::FromI64),
        (CompilerItem::U128, Self::FromPair),
    ];

    /// The constructor method `item` reaches this shape through — the one
    /// mapping between the two, so the producer and the matcher below cannot
    /// come to list different sets.
    fn method(self, item: CompilerItem) -> CompilerItem {
        match (item, self) {
            (CompilerItem::I128, Self::FromI64) => CompilerItem::I128FromI64,
            (CompilerItem::I128, Self::FromU64) => CompilerItem::I128FromU64,
            (CompilerItem::I128, Self::FromPair) => CompilerItem::I128FromPair,
            (CompilerItem::U128, Self::FromU64) => CompilerItem::U128FromU64,
            (CompilerItem::U128, Self::FromI64) => CompilerItem::U128FromI64,
            (CompilerItem::U128, Self::FromPair) => CompilerItem::U128FromPair,
            (item, shape) => panic!("{item} has no {shape:?} constructor"),
        }
    }

    /// The 128-bit pattern `args` denote, in the callee's parameter order and
    /// each read as the raw bits of its declared 64-bit parameter type.
    pub(crate) fn compose(self, args: &[u64]) -> i128 {
        match (self, args) {
            (Self::FromI64, [value]) => i128::from(value.cast_signed()),
            (Self::FromU64, [value]) => i128::from(*value),
            (Self::FromPair, [low, high]) => (i128::from(*high) << 64) | i128::from(*low),
            _ => panic!("wide-int constructor {self:?} takes a different argument count"),
        }
    }
}

/// The wide-int constructor `mangled` names, or `None` for any other callee.
pub(crate) fn classify_ctor(type_table: &TypeTable, mangled: &str) -> Option<WideIntCtor> {
    WideIntCtor::ALL.into_iter().find_map(|(owner, shape)| {
        (method_ref(type_table, shape.method(owner)).name == mangled).then_some(shape)
    })
}

/// `operand as <item>` for an operand that is not itself a wide int, through
/// the `f64`, `i64` or `u64` whose constructor keeps its value as Rust's `as`
/// does: a float saturates, a signed integer sign-extends, and anything else
/// that casts to an integer (unsigned, `bool`, `char`, an enum, flags)
/// zero-extends.
pub(crate) fn create_conversion(
    item: CompilerItem,
    operand: TirExpr,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    let source = operand.type_id;
    let (ctor, via) = if type_table.is_float(source) {
        let ctor = match item {
            CompilerItem::I128 => CompilerItem::I128FromF64,
            CompilerItem::U128 => CompilerItem::U128FromF64,
            other => panic!("{other} is not a wide-integer type"),
        };
        (ctor, TypeTable::F64)
    } else if type_table.is_integer(source) && !type_table.is_unsigned_int(source) {
        (WideIntCtor::FromI64.method(item), TypeTable::I64)
    } else {
        (WideIntCtor::FromU64.method(item), TypeTable::U64)
    };
    let operand = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(operand),
            target_type: via,
        },
        via,
        span,
    );
    ctor_call(ctor, vec![operand], type_id, type_table, span)
}

/// A literal of the wide-integer type `item` carrying the bit pattern `bits`.
/// A value fitting 64 bits goes through `from_i64` / `from_u64`, anything wider
/// through `from_pair(low, high)` — the elaborator's own split for source
/// literals.
pub(crate) fn create_literal(
    item: CompilerItem,
    bits: i128,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    let (low, high) = (bits.cast_unsigned() as u64, (bits >> 64) as u64);
    // Each argument as `(raw bits, declared parameter type)`.
    let (shape, args): (_, Vec<(u64, TypeId)>) = match item {
        CompilerItem::I128 if i64::try_from(bits).is_ok() => {
            (WideIntCtor::FromI64, vec![(low, TypeTable::I64)])
        }
        CompilerItem::I128 => (
            WideIntCtor::FromPair,
            vec![(low, TypeTable::U64), (high, TypeTable::I64)],
        ),
        CompilerItem::U128 if u64::try_from(bits.cast_unsigned()).is_ok() => {
            (WideIntCtor::FromU64, vec![(low, TypeTable::U64)])
        }
        CompilerItem::U128 => (
            WideIntCtor::FromPair,
            vec![(low, TypeTable::U64), (high, TypeTable::U64)],
        ),
        other => panic!("{other} is not a wide-integer type"),
    };
    let args = args
        .into_iter()
        .map(|(value, param_type)| {
            // `repr` is the spelling a dump prints, so it follows the
            // parameter's own signedness.
            let repr = if param_type == TypeTable::I64 {
                value.cast_signed().to_string()
            } else {
                value.to_string()
            };
            TirExpr::new(TirExprKind::IntLiteral { value, repr }, param_type, span)
        })
        .collect();
    ctor_call(shape.method(item), args, type_id, type_table, span)
}

/// A literal of the wide-integer type `item` read from `repr`, an
/// [`TirExprKind::IntLiteral`]'s only operand wide enough to hold 128 bits.
/// Producers spell one bit pattern either way, and a decimal falls in exactly
/// one of the two ranges, so both readings are tried.
pub(crate) fn literal_from_repr(
    item: CompilerItem,
    repr: &str,
    type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    use crate::elaborator::util::{parse_i128_literal, parse_u128_literal};
    let bits = parse_i128_literal(repr)
        .or_else(|_| parse_u128_literal(repr).map(u128::cast_signed))
        .unwrap_or_else(|e| panic!("a wide-int literal reaching `lower` parses: {repr}: {e}"));
    create_literal(item, bits, type_id, type_table, span)
}

/// `left <op> right` for the wide-integer type `item`, as the call into its
/// `Eq` / `Ord` impl that answers the same question. `None` for an operator
/// neither trait covers.
pub(crate) fn compare(
    item: CompilerItem,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Option<TirExpr> {
    use crate::ast::BinaryOp;

    let eq = || {
        trait_method_call(
            item,
            CompilerItem::Eq,
            "eq",
            left,
            right,
            TypeTable::BOOL,
            type_table,
            span,
        )
    };
    let ord = |op| {
        let ordering = type_table
            .borrow_mut()
            .make_compiler_enum(CompilerItem::Ordering);
        let cmp = trait_method_call(
            item,
            CompilerItem::Ord,
            "cmp",
            left,
            right,
            ordering,
            type_table,
            span,
        );
        ord_bool_from_cmp(cmp, op, span, type_table)
    };
    Some(match op {
        TirBinaryOp::Eq => eq(),
        TirBinaryOp::NotEq => not_expr(eq(), span),
        TirBinaryOp::Lt => ord(BinaryOp::Lt),
        TirBinaryOp::Gt => ord(BinaryOp::Gt),
        TirBinaryOp::LtEq => ord(BinaryOp::LtEq),
        TirBinaryOp::GtEq => ord(BinaryOp::GtEq),
        _ => return None,
    })
}

/// `<item>^<trait_item>::<method>(&left, &right)`. Both operands go by
/// reference because that is how the prelude declares every `Eq` / `Ord`
/// method.
#[allow(clippy::too_many_arguments)]
fn trait_method_call(
    item: CompilerItem,
    trait_item: CompilerItem,
    method: &str,
    left: &TirExpr,
    right: &TirExpr,
    result_type: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let by_ref = |expr: &TirExpr| {
        let ref_type = type_table
            .borrow_mut()
            .intern(ResolvedType::Ref(expr.type_id));
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: Box::new(expr.clone()),
            },
            ref_type,
            span,
        )
    };
    let receiver = by_ref(left);
    let arg = by_ref(right);
    let (trait_name, struct_name, module_source) = {
        let tt = type_table.borrow();
        (
            tt.compiler_trait_fq(trait_item),
            tt.compiler_struct_fq_name(item),
            tt.compiler_items().require_struct(item).0.clone(),
        )
    };
    let method_info = LocalMethodName::new(struct_name, Some(trait_name), method.to_string());
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source,
                name: method_info.to_mangled_name(),
                template: None,
                monomorph_info: None,
                method_info: Some(method_info),
            },
            vec![],
            vec![CallArg::new(arg, false)],
        ),
        result_type,
        span,
    )
}
