//! Pure compile-time constant evaluation shared by `niri` (the CTFE
//! interpreter) and the `ValueGraph` constant folder
//! ([`crate::nir_value_graph`]). These are total, interpreter-state-free
//! functions over [`Value`] and `PrimitiveType`, kept in a lower module so the
//! value-graph builder can fold arithmetic without depending on `niri`.

use std::rc::Rc;

use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::Body;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

/// A typed compile-time value produced by the interpreter.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Integer value. `prim` carries the integer type (i8..i64, u8..u64);
    /// `value` is the raw bit pattern, sign-extended for signed types.
    Int { value: u64, prim: PrimitiveType },
    /// Floating-point value. `prim` is `F32` or `F64`. For `F32`, `value`
    /// holds the f32 result widened to f64.
    Float { value: f64, prim: PrimitiveType },
    /// Boolean value.
    Bool(bool),
    /// Unicode scalar value (`char`).
    Char(char),
    /// The null reference. Equality is decided only against another `Null`: a
    /// constant aggregate names a value, not the reference reaching it, so
    /// `null == <aggregate>` stays unevaluated rather than answered `false`.
    Null,
    /// The unit value, `()`.
    Unit,
    /// A struct or tuple whose every field is itself a compile-time value.
    ///
    /// Fields are keyed by `field_index` — the key `FieldAccess`,
    /// `StructLiteral`, and struct patterns all carry — and kept sorted by it,
    /// so structural equality is independent of literal field order. A NIR
    /// aggregate literal always lists every field (the elaborator fills
    /// defaults and rejects omissions), so the field list is complete and
    /// equality is exact.
    ///
    /// Aggregates exist only inside the interpreter: the value pool holds pure
    /// *scalars*, so what reaches the IR is the scalars projected out of them.
    Aggregate {
        type_id: TypeId,
        fields: Rc<[(u32, Value)]>,
    },
    /// A backing array of compile-time values. `String` and `List<T>` need no
    /// case of their own: each is an [`Value::Aggregate`] over one of these
    /// and a length. Read an element against the container's length, not this
    /// array's — a grown container's capacity outruns what it holds.
    Seq {
        type_id: TypeId,
        elements: Rc<[Value]>,
    },
    /// A variant value: which case it holds, and the payload that case carries.
    ///
    /// Keyed by case *name*, the identity a pattern spells — a case index would
    /// have to be resolved through the type table on both sides. A unit case
    /// carries no payload; a multi-field one carries the aggregate its
    /// construction site built, so a binding reads it by field index.
    Variant {
        type_id: TypeId,
        case_name: Rc<str>,
        payload: Option<Rc<Value>>,
    },
}

/// Longest literal the engine turns into a [`Value::Seq`]. Building one walks
/// every element, so an embedded asset would cost more than any fold it could
/// enable; past this it is simply not a constant here.
pub const MAX_SEQ_ELEMENTS: usize = 1024;

/// A backing nobody else holds, to be written through in place.
///
/// `Rc::make_mut` covers `Rc<T>` but not `Rc<[T]>`, so the copy-on-write step
/// is spelled out: a shared backing is copied and the copy handed back, a
/// unique one is handed back where it lies. That is what keeps filling a
/// sequence element by element linear — copying it per write is what makes it
/// quadratic — while a value two locals share still forks on the first write.
fn unshared<T: Clone>(backing: &mut Rc<[T]>) -> &mut [T] {
    if Rc::get_mut(backing).is_none() {
        *backing = backing.iter().cloned().collect();
    }
    Rc::get_mut(backing).expect("the backing was just replaced with a unique one")
}

impl Value {
    /// An aggregate over `fields`, canonicalized to `field_index` order so two
    /// literals listing the same fields in different order compare equal.
    #[must_use]
    pub fn aggregate(type_id: TypeId, mut fields: Vec<(u32, Value)>) -> Self {
        fields.sort_by_key(|(index, _)| *index);
        Self::Aggregate {
            type_id,
            fields: fields.into(),
        }
    }

    /// The value of field `index`, or `None` for a scalar or an absent field.
    #[must_use]
    pub fn field(&self, index: u32) -> Option<&Self> {
        let Self::Aggregate { fields, .. } = self else {
            return None;
        };
        fields
            .binary_search_by_key(&index, |(i, _)| *i)
            .ok()
            .map(|pos| &fields[pos].1)
    }

    /// This aggregate's field `index`, to be written through. `None` for a
    /// scalar or an absent field — a NIR aggregate literal lists every field,
    /// so an absent one means the value is not the aggregate the write assumed.
    pub fn field_mut(&mut self, index: u32) -> Option<&mut Self> {
        let Self::Aggregate { fields, .. } = self else {
            return None;
        };
        let pos = fields.binary_search_by_key(&index, |(i, _)| *i).ok()?;
        Some(&mut unshared(fields)[pos].1)
    }

    /// The value at `path` inside this one, to be written through. `None` when
    /// the path does not reach a field the value has.
    pub fn place_mut(&mut self, path: &[u32]) -> Option<&mut Self> {
        path.iter().try_fold(self, |v, i| v.field_mut(*i))
    }

    /// Write `value` into element `index`. `None` for a non-sequence or an
    /// index past the end, where the write traps at run time — and nothing is
    /// written in either case.
    pub fn set_element(&mut self, index: u64, value: Self) -> Option<()> {
        let Self::Seq { elements, .. } = self else {
            return None;
        };
        let index = usize::try_from(index).ok()?;
        if index >= elements.len() {
            return None;
        }
        unshared(elements)[index] = value;
        Some(())
    }

    /// Write `len` of `source`'s elements from `from` at `at`. `None` for a
    /// non-sequence or a run either side cannot supply, where the copy traps at
    /// run time — the bounds are settled before anything is written, so a
    /// refused copy leaves the destination as it was.
    pub fn set_run(&mut self, at: u64, source: &Self, from: u64, len: u64) -> Option<()> {
        let Self::Seq {
            elements: source, ..
        } = source
        else {
            return None;
        };
        let (at, from, len) = (
            usize::try_from(at).ok()?,
            usize::try_from(from).ok()?,
            usize::try_from(len).ok()?,
        );
        let source = source.get(from..from.checked_add(len)?)?.to_vec();
        let Self::Seq { elements, .. } = self else {
            return None;
        };
        let end = at.checked_add(len)?;
        if end > elements.len() {
            return None;
        }
        unshared(elements)[at..end].clone_from_slice(&source);
        Some(())
    }

    /// The value `array.new_default` leaves in an element of this primitive
    /// type. `None` for the widths the value model does not carry, and for a
    /// reference element, whose default is null.
    #[must_use]
    pub fn default_of(prim: PrimitiveType) -> Option<Self> {
        match prim {
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64 => Some(Self::Int { value: 0, prim }),
            PrimitiveType::F32 | PrimitiveType::F64 => Some(Self::Float { value: 0.0, prim }),
            PrimitiveType::Bool => Some(Self::Bool(false)),
            PrimitiveType::Char => Some(Self::Char('\0')),
            PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128 => None,
        }
    }

    /// `None` when longer than [`MAX_SEQ_ELEMENTS`].
    #[must_use]
    pub fn seq(type_id: TypeId, elements: Vec<Value>) -> Option<Self> {
        (elements.len() <= MAX_SEQ_ELEMENTS).then(|| Self::Seq {
            type_id,
            elements: elements.into(),
        })
    }

    /// The element at `index`. `None` past the end, where a read traps.
    #[must_use]
    pub fn element(&self, index: u64) -> Option<&Self> {
        let Self::Seq { elements, .. } = self else {
            return None;
        };
        elements.get(usize::try_from(index).ok()?)
    }

    #[must_use]
    pub fn seq_len(&self) -> Option<usize> {
        match self {
            Self::Seq { elements, .. } => Some(elements.len()),
            Self::Int { .. }
            | Self::Float { .. }
            | Self::Bool(_)
            | Self::Char(_)
            | Self::Null
            | Self::Unit
            | Self::Aggregate { .. }
            | Self::Variant { .. } => None,
        }
    }

    /// Whether the value can be promoted into a pure-value operand. Aggregates
    /// and sequences cannot: the pool models scalars only.
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        !matches!(
            self,
            Self::Aggregate { .. } | Self::Seq { .. } | Self::Variant { .. }
        )
    }

    /// Whether one value may stand in for the other — the question a rewrite
    /// asks before replacing an expression, which is not the question `==`
    /// answers.
    ///
    /// `PartialEq` models the program's own `==`, so it follows IEEE and holds
    /// for `-0.0` and `0.0`. A program tells those apart (`1.0 / x` alone
    /// does), so substituting either changes what it computes. Two NaNs are
    /// equal under neither question, which leaves them out of reach.
    #[must_use]
    pub fn denotes_same(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Float {
                    value: a,
                    prim: a_prim,
                },
                Self::Float {
                    value: b,
                    prim: b_prim,
                },
            ) => a_prim == b_prim && a == b && a.is_sign_negative() == b.is_sign_negative(),
            (
                Self::Aggregate {
                    type_id: a_type,
                    fields: a_fields,
                },
                Self::Aggregate {
                    type_id: b_type,
                    fields: b_fields,
                },
            ) => {
                a_type == b_type
                    && a_fields.len() == b_fields.len()
                    && a_fields.iter().zip(b_fields.iter()).all(
                        |((a_index, a_value), (b_index, b_value))| {
                            a_index == b_index && a_value.denotes_same(b_value)
                        },
                    )
            }
            (
                Self::Seq {
                    type_id: a_type,
                    elements: a_elements,
                },
                Self::Seq {
                    type_id: b_type,
                    elements: b_elements,
                },
            ) => {
                a_type == b_type
                    && a_elements.len() == b_elements.len()
                    && a_elements
                        .iter()
                        .zip(b_elements.iter())
                        .all(|(a, b)| a.denotes_same(b))
            }
            (Self::Null, Self::Null) | (Self::Unit, Self::Unit) => true,
            (Self::Int { .. } | Self::Bool(_) | Self::Char(_), _) => self == other,
            (Self::Null | Self::Unit, _) => false,
            (
                Self::Float { .. }
                | Self::Aggregate { .. }
                | Self::Seq { .. }
                | Self::Variant { .. },
                _,
            ) => false,
        }
    }

    /// Returns the raw integer bit pattern, or `None` if not an int.
    #[must_use]
    pub fn as_int(&self) -> Option<(u64, PrimitiveType)> {
        match self {
            Self::Int { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the raw float value and width, or `None` if not a float.
    #[must_use]
    pub fn as_float(&self) -> Option<(f64, PrimitiveType)> {
        match self {
            Self::Float { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the boolean value, or `None` if not a bool.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the char value, or `None` if not a char.
    #[must_use]
    pub fn as_char(&self) -> Option<char> {
        match self {
            Self::Char(c) => Some(*c),
            _ => None,
        }
    }

    /// Render the value as a NIR-compatible literal repr string.
    ///
    /// Scalars only. An aggregate has no literal form in NIR but can reach the
    /// pool, so an arbitrary `Value` must pass [`Self::is_scalar`] first.
    #[must_use]
    pub fn format_repr(&self) -> String {
        match self {
            Self::Int { value, prim } => format_int_repr(*value, *prim),
            Self::Float { value, .. } => format_float_repr(*value),
            Self::Bool(b) => b.to_string(),
            Self::Char(c) => format_char_repr(*c),
            Self::Null => "null".to_string(),
            Self::Unit => "()".to_string(),
            Self::Aggregate { .. } | Self::Seq { .. } | Self::Variant { .. } => {
                panic!(
                    "an aggregate, sequence, or variant value has no NIR literal repr; none enters the value pool"
                )
            }
        }
    }

    /// Project an [`Operand`] to the constant it denotes. Constants live in
    /// the `ValuePool`, so only `Operand::Value` can be one.
    #[must_use]
    pub fn from_operand(
        body: &Body,
        op: crate::nir_arena::Operand,
        type_table: &TypeTable,
    ) -> Option<Self> {
        let v = op.as_value()?;
        let ty = body.values.type_of(v)?;
        crate::nir_value_graph::value_kind_to_const(body.values.kind(v), prim_of(ty, type_table))
    }
}

/// Evaluate a binary op on two compile-time values.
pub(crate) fn eval_binary(left: Value, op: NirBinaryOp, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Bool(l), Value::Bool(r)) => eval_bool_binary(l, op, r),
        (Value::Char(l), Value::Char(r)) => eval_char_binary(l, op, r),
        (Value::Null, Value::Null) | (Value::Unit, Value::Unit) => eval_singleton_binary(op),
        (Value::Float { value: l, prim: lp }, Value::Float { value: r, prim: rp }) if lp == rp => {
            eval_float_binary(l, op, r, lp)
        }
        (Value::Int { value: l, prim: lp }, Value::Int { value: r, prim: rp }) if lp == rp => {
            eval_int_binary(l, op, r, lp)
        }
        _ => None,
    }
}

/// Evaluate a unary op on a compile-time value.
pub(crate) fn eval_unary(op: NirUnaryOp, operand: Value) -> Option<Value> {
    match op {
        NirUnaryOp::Neg => match operand {
            Value::Int { value, prim } => {
                eval_int_neg(value, prim).map(|v| Value::Int { value: v, prim })
            }
            Value::Float { value, prim } => {
                let negated = f64::from_bits(value.to_bits() ^ (1u64 << 63));
                Some(Value::Float {
                    value: negated,
                    prim,
                })
            }
            Value::Bool(_)
            | Value::Char(_)
            | Value::Null
            | Value::Unit
            | Value::Aggregate { .. }
            | Value::Seq { .. }
            | Value::Variant { .. } => None,
        },
        NirUnaryOp::Not => match operand {
            Value::Bool(b) => Some(Value::Bool(!b)),
            _ => None,
        },
        NirUnaryOp::BitNot => match operand {
            Value::Int { value, prim } => Some(Value::Int {
                value: truncate_int(!value, prim),
                prim,
            }),
            _ => None,
        },
        NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref => None,
    }
}

/// Evaluate an `as` cast at compile time.
///
/// Source values are the lattice-resolved [`Value`] of the cast input;
/// `target` is the destination primitive (resolved from the cast node's
/// `type_id`). Returns `None` for unsupported pairs — the caller maps
/// that to [`Lattice::NonConst`] so the runtime cast still happens, no
/// bogus value gets folded in.
///
/// The supported set mirrors what the elaborator permits in source:
///
/// - `Int` source ↦ Int (already supported), Float, Char (only when
///   source is `U8` per [`expr.rs`]'s `u8 as char` carve-out).
/// - `Float` source ↦ Float, Int (saturating, matching Wasm's
///   `*.trunc_sat_*` semantics — Rust's `as` since 1.45 implements the
///   same rounding/saturation rules so we forward to it).
/// - `Bool` source ↦ Int (0/1), Float (0.0/1.0). Bool → Bool is the
///   identity.
/// - `Char` source ↦ Int (codepoint, then truncated). Char → Char is the
///   identity.
///
/// 128-bit (`I128`/`U128`) and SIMD (`V128`) targets are reachable here
/// (they are valid `Primitive` variants) but currently unsupported and
/// fall through to `None`.
pub(crate) fn eval_cast(source: Value, target: PrimitiveType) -> Option<Value> {
    let int_target = is_int_prim(target);
    let float_target = matches!(target, PrimitiveType::F32 | PrimitiveType::F64);
    match source {
        // The source `prim` is irrelevant for int→int because
        // `truncate_int` operates on the already sign- or zero-extended
        // u64 representation set up at construction time.
        Value::Int { value, .. } if int_target => Some(Value::Int {
            value: truncate_int(value, target),
            prim: target,
        }),
        Value::Int { value, prim } if float_target => Some(int_to_float(value, prim, target)),
        // Only `u8 as char` is permitted by the elaborator; every u8 is a
        // valid Unicode scalar, so `char::from(u8)` is total.
        Value::Int {
            value,
            prim: PrimitiveType::U8,
        } if target == PrimitiveType::Char => Some(Value::Char(char::from(value as u8))),

        Value::Float { value, prim } if float_target => Some(float_to_float(value, prim, target)),
        Value::Float { value, prim } if int_target => float_to_int(value, prim, target),

        Value::Bool(b) if int_target => Some(Value::Int {
            value: u64::from(b),
            prim: target,
        }),
        Value::Bool(b) if float_target => Some(Value::Float {
            value: if b { 1.0 } else { 0.0 },
            prim: target,
        }),
        Value::Bool(b) if target == PrimitiveType::Bool => Some(Value::Bool(b)),

        Value::Char(c) if int_target => Some(Value::Int {
            value: truncate_int(u64::from(c as u32), target),
            prim: target,
        }),
        Value::Char(c) if target == PrimitiveType::Char => Some(Value::Char(c)),

        _ => None,
    }
}

/// True for the eight integer primitives the engine models. 128-bit
/// (`I128`/`U128`) is intentionally excluded — those types lower to
/// stdlib calls in source, not a `Cast` node niri can fold.
pub(crate) fn is_int_prim(p: PrimitiveType) -> bool {
    matches!(
        p,
        PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64,
    )
}

/// Convert an integer (held as the sign-extended u64 bit pattern of
/// `prim`) into a float of `target` width. Signed widths are routed
/// through `i64` so the negative range survives; unsigned widths use
/// `u64` directly. F32 results are widened back to f64 so the engine's
/// canonical [`Value::Float`] repr is preserved.
pub(crate) fn int_to_float(value: u64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let f = if is_signed_int(prim) {
        // `truncate_int` already sign-extended `value` into the i64 range
        // for I8/I16/I32, and an I64 value's u64 bits round-trip through
        // `as i64 as f64`.
        match target {
            PrimitiveType::F32 => f64::from((value as i64) as f32),
            _ => (value as i64) as f64,
        }
    } else {
        match target {
            PrimitiveType::F32 => f64::from(value as f32),
            _ => value as f64,
        }
    };
    Value::Float {
        value: f,
        prim: target,
    }
}

/// Float ↔ float conversion. Widening (f32 → f64) is a no-op on the
/// stored f64 since every f32 is exactly representable; narrowing
/// (f64 → f32) routes through `as f32` to apply the rounding step,
/// then re-widens to f64 for storage. Same-width casts are the identity.
pub(crate) fn float_to_float(value: f64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let v = match (prim, target) {
        (PrimitiveType::F64, PrimitiveType::F32) => f64::from(value as f32),
        (PrimitiveType::F32 | PrimitiveType::F64, PrimitiveType::F64)
        | (PrimitiveType::F32, PrimitiveType::F32) => value,
        _ => panic!("float_to_float: non-float prim ({prim:?} → {target:?})"),
    };
    Value::Float {
        value: v,
        prim: target,
    }
}

/// The integer a float → int cast produces, or `None` where the cast traps.
///
/// `target` must be one of the i8..u64 primitives, as [`eval_cast`]'s dispatch
/// guarantees; anything else panics rather than fabricate a value.
///
/// An F32 source truncates at f32 precision to match the runtime cast; the
/// stored f64 is bit-equivalent, so the f64 path is a no-op widening otherwise.
pub(crate) fn float_to_int(
    value: f64,
    prim: PrimitiveType,
    target: PrimitiveType,
) -> Option<Value> {
    let value = match prim {
        PrimitiveType::F32 => f64::from(value as f32),
        _ => value,
    };
    Some(Value::Int {
        value: truncate_int(trunc_to_int(value, target)?, target),
        prim: target,
    })
}

/// The trapping float → int truncation wasm performs, as the sign- or
/// zero-extended bit pattern of the intermediate. `None` where it traps: a NaN,
/// an infinity, or a truncation that leaves the intermediate's range.
///
/// The intermediate is what decides both, and it is not the target: a cast to a
/// narrower integer truncates through i32 and wraps the result down, so
/// `300.7 as i8` is 44 rather than the saturated 127.
fn trunc_to_int(value: f64, target: PrimitiveType) -> Option<u64> {
    if !value.is_finite() {
        return None;
    }
    let truncated = value.trunc();
    match target {
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 => (-2_147_483_648.0
            ..=2_147_483_647.0)
            .contains(&truncated)
            .then_some(i64::from(truncated as i32) as u64),
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 => (0.0..=4_294_967_295.0)
            .contains(&truncated)
            .then_some(u64::from(truncated as u32)),
        PrimitiveType::I64 => (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
            .contains(&truncated)
            .then_some(truncated as i64 as u64),
        PrimitiveType::U64 => (0.0..18_446_744_073_709_551_616.0)
            .contains(&truncated)
            .then_some(truncated as u64),
        PrimitiveType::F32
        | PrimitiveType::F64
        | PrimitiveType::Bool
        | PrimitiveType::Char
        | PrimitiveType::I128
        | PrimitiveType::U128
        | PrimitiveType::V128 => panic!("trunc_to_int: non-integer target {target:?}"),
    }
}

/// Equality between two single-inhabitant values: `null` with `null`, `()`
/// with `()`. Every other operator is meaningless on them.
pub(crate) fn eval_singleton_binary(op: NirBinaryOp) -> Option<Value> {
    match op {
        NirBinaryOp::Eq => Some(Value::Bool(true)),
        NirBinaryOp::NotEq => Some(Value::Bool(false)),
        _ => None,
    }
}

pub(crate) fn eval_bool_binary(l: bool, op: NirBinaryOp, r: bool) -> Option<Value> {
    match op {
        NirBinaryOp::And => Some(Value::Bool(l && r)),
        NirBinaryOp::Or => Some(Value::Bool(l || r)),
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        // bool implements Ord with `false < true`. Spelled with `&&`
        // rather than `<` to satisfy clippy's `bool_comparison` lint
        // without tripping `needless_bitwise_bool`.
        NirBinaryOp::Lt => Some(Value::Bool(!l && r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l && !r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

/// `char` comparisons. char implements `Eq` and `Ord` (codepoint
/// order); arithmetic / bitwise ops are not defined.
pub(crate) fn eval_char_binary(l: char, op: NirBinaryOp, r: char) -> Option<Value> {
    match op {
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        NirBinaryOp::Lt => Some(Value::Bool(l < r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l > r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

pub(crate) fn eval_int_binary(
    lval: u64,
    op: NirBinaryOp,
    rval: u64,
    prim: PrimitiveType,
) -> Option<Value> {
    match op {
        NirBinaryOp::Add => Some(Value::Int {
            value: truncate_int(lval.wrapping_add(rval), prim),
            prim,
        }),
        NirBinaryOp::Sub => Some(Value::Int {
            value: truncate_int(lval.wrapping_sub(rval), prim),
            prim,
        }),
        NirBinaryOp::Mul => Some(Value::Int {
            value: truncate_int(lval.wrapping_mul(rval), prim),
            prim,
        }),
        NirBinaryOp::Div => eval_int_div(lval, rval, prim).map(|value| Value::Int { value, prim }),
        NirBinaryOp::Mod => eval_int_mod(lval, rval, prim).map(|value| Value::Int { value, prim }),

        NirBinaryOp::Eq
        | NirBinaryOp::NotEq
        | NirBinaryOp::Lt
        | NirBinaryOp::LtEq
        | NirBinaryOp::Gt
        | NirBinaryOp::GtEq => Some(Value::Bool(eval_int_cmp(lval, op, rval, prim))),

        NirBinaryOp::BitAnd => Some(Value::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        NirBinaryOp::BitOr => Some(Value::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        NirBinaryOp::BitXor => Some(Value::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        NirBinaryOp::Shl => Some(Value::Int {
            value: eval_int_shl(lval, rval, prim),
            prim,
        }),
        NirBinaryOp::Shr => Some(Value::Int {
            value: eval_int_shr(lval, rval, prim),
            prim,
        }),

        NirBinaryOp::And | NirBinaryOp::Or | NirBinaryOp::RefEq | NirBinaryOp::RefNotEq => None,
    }
}

pub(crate) fn eval_int_cmp(lval: u64, op: NirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    if is_signed_int(prim) {
        let l = lval as i64;
        let r = rval as i64;
        match op {
            NirBinaryOp::Eq => l == r,
            NirBinaryOp::NotEq => l != r,
            NirBinaryOp::Lt => l < r,
            NirBinaryOp::LtEq => l <= r,
            NirBinaryOp::Gt => l > r,
            NirBinaryOp::GtEq => l >= r,
            _ => unreachable!(),
        }
    } else {
        match op {
            NirBinaryOp::Eq => lval == rval,
            NirBinaryOp::NotEq => lval != rval,
            NirBinaryOp::Lt => lval < rval,
            NirBinaryOp::LtEq => lval <= rval,
            NirBinaryOp::Gt => lval > rval,
            NirBinaryOp::GtEq => lval >= rval,
            _ => unreachable!(),
        }
    }
}

pub(crate) fn eval_int_shl(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    truncate_int(lval.wrapping_shl(shift), prim)
}

pub(crate) fn eval_int_shr(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    if is_signed_int(prim) {
        let result = (lval as i64).wrapping_shr(shift);
        truncate_int(result as u64, prim)
    } else {
        truncate_int(lval.wrapping_shr(shift), prim)
    }
}

pub(crate) fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval / rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_div(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_div(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

pub(crate) fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval % rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_rem(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_rem(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_rem(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_rem(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

pub(crate) fn eval_int_neg(value: u64, prim: PrimitiveType) -> Option<u64> {
    match prim {
        PrimitiveType::I8 => {
            let result = (value as i8).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (value as i16).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (value as i32).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            let result = (value as i64).wrapping_neg();
            Some(result as u64)
        }
        _ => None,
    }
}

pub(crate) fn eval_float_binary(
    lval: f64,
    op: NirBinaryOp,
    rval: f64,
    prim: PrimitiveType,
) -> Option<Value> {
    match prim {
        PrimitiveType::F32 => eval_f32_binary(lval, op, rval),
        PrimitiveType::F64 => eval_f64_binary(lval, op, rval),
        _ => None,
    }
}

pub(crate) fn eval_f64_binary(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        NirBinaryOp::Add => non_nan_float(lval + rval, PrimitiveType::F64),
        NirBinaryOp::Sub => non_nan_float(lval - rval, PrimitiveType::F64),
        NirBinaryOp::Mul => non_nan_float(lval * rval, PrimitiveType::F64),
        NirBinaryOp::Div => non_nan_float(lval / rval, PrimitiveType::F64),
        _ => eval_float_comparison(lval, op, rval),
    }
}

pub(crate) fn eval_f32_binary(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        NirBinaryOp::Add => non_nan_float(f64::from(l + r), PrimitiveType::F32),
        NirBinaryOp::Sub => non_nan_float(f64::from(l - r), PrimitiveType::F32),
        NirBinaryOp::Mul => non_nan_float(f64::from(l * r), PrimitiveType::F32),
        NirBinaryOp::Div => non_nan_float(f64::from(l / r), PrimitiveType::F32),
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        NirBinaryOp::Lt => Some(Value::Bool(l < r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l > r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

pub(crate) fn eval_float_comparison(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        NirBinaryOp::Eq => Some(Value::Bool(lval == rval)),
        NirBinaryOp::NotEq => Some(Value::Bool(lval != rval)),
        NirBinaryOp::Lt => Some(Value::Bool(lval < rval)),
        NirBinaryOp::LtEq => Some(Value::Bool(lval <= rval)),
        NirBinaryOp::Gt => Some(Value::Bool(lval > rval)),
        NirBinaryOp::GtEq => Some(Value::Bool(lval >= rval)),
        _ => None,
    }
}

pub(crate) fn non_nan_float(value: f64, prim: PrimitiveType) -> Option<Value> {
    if value.is_nan() {
        return None;
    }
    Some(Value::Float { value, prim })
}

// ──────────────────────────────────────────────────────────────────────────────
// Type queries, truncation, formatting
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn is_signed_int(prim: PrimitiveType) -> bool {
    matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    )
}

pub(crate) fn int_bit_width(prim: PrimitiveType) -> u32 {
    match prim {
        PrimitiveType::I8 | PrimitiveType::U8 => 8,
        PrimitiveType::I16 | PrimitiveType::U16 => 16,
        PrimitiveType::I32 | PrimitiveType::U32 => 32,
        PrimitiveType::I64 | PrimitiveType::U64 => 64,
        _ => 32,
    }
}

pub(crate) fn is_f32_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(PrimitiveType::F32)
    )
}

/// Resolve any primitive type from a [`TypeId`]. Used by the cast path
/// where the target may be int / float / bool / char (i128/u128/v128
/// are returned but [`eval_cast`] declines to fold them) and by
/// `IntLiteral` lattice resolution after a [`is_int_prim`] filter.
pub(crate) fn prim_of(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(p) => Some(*p),
        _ => None,
    }
}

/// Truncate / sign-extend an integer bit pattern to fit the target prim.
#[must_use]
pub(crate) fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
    match prim {
        PrimitiveType::U8 => value & 0xFF,
        PrimitiveType::U16 => value & 0xFFFF,
        PrimitiveType::U32 => value & 0xFFFF_FFFF,
        PrimitiveType::U64 => value,
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value,
        _ => value,
    }
}

/// Render an integer bit pattern as decimal text, signed when the prim
/// is signed.
#[must_use]
pub(crate) fn format_int_repr(value: u64, prim: PrimitiveType) -> String {
    if is_signed_int(prim) {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Render a `char` as a Wado-friendly literal repr (`'A'`, `'\n'`,
/// `'\u{1F600}'`, …). Used when re-emitting a folded `char` value as a
/// `ExprKind::CharLiteral`.
#[must_use]
pub(crate) fn format_char_repr(c: char) -> String {
    match c {
        '\\' => "'\\\\'".to_string(),
        '\'' => "'\\''".to_string(),
        '\n' => "'\\n'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\t' => "'\\t'".to_string(),
        '\0' => "'\\0'".to_string(),
        c if c.is_ascii_graphic() || c == ' ' => format!("'{c}'"),
        c => format!("'\\u{{{:X}}}'", c as u32),
    }
}

/// Render a float as a Wado-friendly literal repr (`3.25`, `0.0`,
/// `Infinity`, `-Infinity`, …). Trailing `.0` is appended to integral
/// values so the result parses back as a float literal.
#[must_use]
pub(crate) fn format_float_repr(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    let s = value.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}
