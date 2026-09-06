//! TIR builders for `i128` / `u128` values — both are prelude structs, so a
//! literal is a constructor call and a comparison an `Eq` / `Ord` call.
//!
//! At the `lower::` top level for callers either side of the planner /
//! translator boundary: the optimizer and `wir_build` match on the `Call` shape,
//! so every producer must emit the same one.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId,
    TypeTable,
};
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

/// Which wide-int constructor a call names, and how its arguments compose into
/// the 128-bit pattern. [`classify_ctor`] recognises exactly the calls
/// [`create_literal`] emits, so a consumer reading a wide-int literal back
/// cannot drift from the producer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WideIntCtor {
    /// `i128::from_i64(v)` — sign-extends.
    FromI64,
    /// `u128::from_u64(v)` — zero-extends.
    FromU64,
    /// `<i128|u128>::from_pair(low, high)` — the halves verbatim.
    FromPair,
}

impl WideIntCtor {
    /// Every `(owner, shape)` pair a wide-int literal can take.
    const ALL: [(CompilerItem, Self); 4] = [
        (CompilerItem::I128, Self::FromI64),
        (CompilerItem::I128, Self::FromPair),
        (CompilerItem::U128, Self::FromU64),
        (CompilerItem::U128, Self::FromPair),
    ];

    /// The constructor method `item` reaches this shape through — the one
    /// mapping between the two, so the producer and the matcher below cannot
    /// come to list different sets.
    fn method(self, item: CompilerItem) -> CompilerItem {
        match (item, self) {
            (CompilerItem::I128, Self::FromI64) => CompilerItem::I128FromI64,
            (CompilerItem::I128, Self::FromPair) => CompilerItem::I128FromPair,
            (CompilerItem::U128, Self::FromU64) => CompilerItem::U128FromU64,
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
        let ctor = ctor_ref(type_table, owner, shape.method(owner));
        let name = LocalMethodName::new(ctor.type_name, None, ctor.method_name);
        (name.to_mangled_name() == mangled).then_some(shape)
    })
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
    let ctor = ctor_ref(type_table, item, shape.method(item));
    let method_info = LocalMethodName::new(ctor.type_name, None, ctor.method_name);
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ctor.module_source,
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: args
                .into_iter()
                .map(|(value, param_type)| {
                    // `repr` is the spelling a dump prints, so it follows the
                    // parameter's own signedness.
                    let repr = if param_type == TypeTable::I64 {
                        value.cast_signed().to_string()
                    } else {
                        value.to_string()
                    };
                    CallArg::new(
                        TirExpr::new(TirExprKind::IntLiteral { value, repr }, param_type, span),
                        false,
                    )
                })
                .collect(),
            has_receiver: false,
        },
        type_id,
        span,
    )
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
        crate::elaborator::reify::ord_bool_from_cmp(cmp, op, span, type_table)
    };
    Some(match op {
        TirBinaryOp::Eq => eq(),
        TirBinaryOp::NotEq => TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(eq()),
            },
            TypeTable::BOOL,
            span,
        ),
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
