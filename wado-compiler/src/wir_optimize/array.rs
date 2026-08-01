//! List optimization passes for WIR.
//!
//! - **Constant array data promotion**: `ArrayNewFixed` of constants → `ArrayNewData`,
//!   when packing encodes smaller than the inline operands.
//! - **Large array literal splitting**: `array.new_fixed` (>= threshold) → `array.new_default` + sets.
//!
//! List literals reach WIR already as `ArrayNewFixed`: the NIR
//! `optimize::array_literal` pass materializes `ExprKind::ArrayLiteral` from
//! the `SequenceLiteralBuilder` push sequence, and `wir_build` lowers it
//! directly. The former WIR-level `collapse_array_push_sequences` that
//! reconstructed `ArrayNewFixed` from inlined `List::push` chains is therefore
//! retired; the passes below consume the `ArrayNewFixed` it used to produce.

use crate::wir::{WirData, WirInstr, WirPackage, WirType, WirTypeDef};
use crate::wir_visitor::WirMutVisitor;

/// Minimum element count to trigger `array.new_data` promotion. Arrays with
/// fewer constant elements keep using `array.new_fixed` whatever their
/// elements encode to — below this the fixed form's per-array overhead is what
/// dominates, not the operands.
pub(crate) const ARRAY_NEW_DATA_THRESHOLD: usize = 128;

/// One `array.new_fixed` operand as the emitter will encode it.
#[derive(Clone, Copy)]
pub(crate) enum ConstOperand {
    I32(i32),
    I64(i64),
    F32,
    F64,
}

impl ConstOperand {
    /// Encoded size: the `T.const` opcode byte plus its immediate — signed
    /// LEB128 for the integer forms, a fixed 4 / 8 bytes for the float ones.
    pub(crate) fn encoded_bytes(self) -> usize {
        let immediate = match self {
            Self::I32(v) => signed_leb128_len(i64::from(v)),
            Self::I64(v) => signed_leb128_len(v),
            Self::F32 => 4,
            Self::F64 => 8,
        };
        1 + immediate
    }
}

fn signed_leb128_len(mut value: i64) -> usize {
    let mut len = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        len += 1;
        let sign_bit_set = byte & 0x40 != 0;
        if (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set) {
            return len;
        }
    }
}

/// Whether packing `count` elements of `elem_width` bytes into a passive data
/// segment is worth it, given what the same elements cost as inline
/// `array.new_fixed` operands.
///
/// A data segment stores every element at its full width, while an operand is
/// LEB128-compressed: a `List<i32>` of small values costs 3 bytes per element
/// inline but 4 packed, so promoting it *grows* the module — and, because
/// `array.new_data` is not a Wasm constant instruction, also demotes a global
/// that would otherwise have become an eager constant to a runtime-initialized
/// one.
///
/// Past [`ARRAY_NEW_FIXED_LIMIT`] the comparison is moot: the alternative is
/// not `array.new_fixed` at all but the `array.new_default` + N × `array.set`
/// build sequence `split_large_array_literals` produces, which is larger than
/// either and is what the limit exists to avoid.
pub(crate) fn data_promotion_pays(
    count: usize,
    elem_width: usize,
    fixed_operand_bytes: usize,
) -> bool {
    count >= ARRAY_NEW_DATA_THRESHOLD
        && (count > ARRAY_NEW_FIXED_LIMIT || count * elem_width < fixed_operand_bytes)
}

/// Promote constant primitive `ArrayNewFixed` to `ArrayNewData`.
///
/// When all elements of an `ArrayNewFixed` are compile-time constants of a
/// primitive type, and packing them encodes smaller than the inline operands
/// ([`data_promotion_pays`]), packs the values into a passive data segment and
/// replaces the instruction with `ArrayNewData`.
pub(super) fn promote_constant_arrays_to_data(module: &mut WirPackage) {
    // Collect element types for array type defs so we can look them up without
    // borrowing `module.types` while mutating other fields.
    let array_elem_types: Vec<Option<WirType>> = module
        .types
        .iter()
        .map(|td| {
            if let WirTypeDef::Array(a) = td {
                Some(a.element_type.clone())
            } else {
                None
            }
        })
        .collect();

    let mut visitor = PromoteConstantArrays {
        array_elem_types: &array_elem_types,
        data: &mut module.data,
    };
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                visitor.visit_instr(instr);
            }
        }
    }

    // Also check global initializers (e.g., `global ITEMS: List<i32> = [1,2,3]`).
    for global in &mut module.globals {
        visitor.visit_instr(&mut global.init);
    }
}

struct PromoteConstantArrays<'a> {
    array_elem_types: &'a [Option<WirType>],
    data: &'a mut Vec<WirData>,
}

impl WirMutVisitor for PromoteConstantArrays<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        // Recurse into children first (bottom-up).
        self.walk_instr(instr);

        // Check if THIS instruction is an eligible ArrayNewFixed.
        if let WirInstr::ArrayNewFixed { type_id, elements } = instr
            && elements.len() >= ARRAY_NEW_DATA_THRESHOLD
        {
            let arr_type_idx = type_id.index() as usize;
            if let Some(Some(elem_type)) = self.array_elem_types.get(arr_type_idx)
                && let Some(bytes) = try_pack_constant_elements(elem_type, elements)
                && data_promotion_pays(
                    elements.len(),
                    element_byte_width(elem_type).expect("packed elements have a width"),
                    fixed_operand_bytes(elements),
                )
            {
                let data_index = u32::try_from(self.data.len()).expect("too many data segments");
                let len = i32::try_from(elements.len()).expect("array length fits i32");
                self.data.push(WirData {
                    bytes,
                    offset: None, // passive segment
                });
                *instr = WirInstr::ArrayNewData {
                    type_id: type_id.clone(),
                    data_index,
                    offset: Box::new(WirInstr::I32Const(0)),
                    len: Box::new(WirInstr::I32Const(len)),
                };
            }
        }
    }
}

/// What `elements` cost as inline `array.new_fixed` operands. Only reached for
/// an element list `try_pack_constant_elements` already accepted, so every
/// element is one of the constant forms.
fn fixed_operand_bytes(elements: &[WirInstr]) -> usize {
    elements
        .iter()
        .map(|e| {
            let operand = match e {
                WirInstr::I32Const(v) => ConstOperand::I32(*v),
                WirInstr::I64Const(v) => ConstOperand::I64(*v),
                WirInstr::F32Const(_) => ConstOperand::F32,
                WirInstr::F64Const(_) => ConstOperand::F64,
                other => panic!("[WIR] packed array element is not a constant: {other:?}"),
            };
            operand.encoded_bytes()
        })
        .sum()
}

/// Try to pack all elements into a byte buffer for `array.new_data`.
///
/// Returns `Some(bytes)` if every element is a compile-time constant matching
/// the expected element type. Returns `None` if any element is non-constant
/// or the element type is not a packable primitive.
fn try_pack_constant_elements(element_type: &WirType, elements: &[WirInstr]) -> Option<Vec<u8>> {
    let byte_width = element_byte_width(element_type)?;
    let mut bytes = Vec::with_capacity(elements.len() * byte_width);

    for elem in elements {
        encode_constant_element(element_type, elem, &mut bytes)?;
    }

    Some(bytes)
}

/// Storage byte width of a packable element, keyed on the NIR primitive rather
/// than the WIR type — the same question [`element_byte_width`] answers after
/// lowering, asked by `const_object_globalization` before it. `bool` is not a
/// `PrimitiveType`; an enum or flags element is four bytes, like the `u32` it
/// lowers to.
pub(crate) fn primitive_byte_width(prim: crate::tir::PrimitiveType) -> Option<usize> {
    use crate::tir::PrimitiveType as P;
    Some(match prim {
        P::I8 | P::U8 => 1,
        P::I16 | P::U16 => 2,
        P::I32 | P::U32 | P::F32 => 4,
        P::I64 | P::U64 | P::F64 => 8,
        _ => return None,
    })
}

/// Returns the storage byte width for a primitive element type in a data segment,
/// or `None` for non-primitive types.
fn element_byte_width(ty: &WirType) -> Option<usize> {
    match ty {
        WirType::I8 | WirType::U8 | WirType::Bool => Some(1),
        WirType::I16 | WirType::U16 => Some(2),
        WirType::I32
        | WirType::U32
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. } => Some(4),
        WirType::I64 | WirType::U64 => Some(8),
        WirType::F32 => Some(4),
        WirType::F64 => Some(8),
        _ => None,
    }
}

/// Encode a single constant WIR instruction into little-endian bytes.
/// Returns `None` if the instruction is not a matching constant.
fn encode_constant_element(
    element_type: &WirType,
    instr: &WirInstr,
    bytes: &mut Vec<u8>,
) -> Option<()> {
    match (element_type, instr) {
        // 1-byte types: i8, u8, bool (stored as I32Const in WIR)
        (WirType::I8 | WirType::U8 | WirType::Bool, WirInstr::I32Const(v)) => {
            bytes.push(v.cast_unsigned() as u8);
        }
        // 2-byte types: i16, u16 (stored as I32Const in WIR)
        (WirType::I16 | WirType::U16, WirInstr::I32Const(v)) => {
            bytes.extend_from_slice(&(v.cast_unsigned() as u16).to_le_bytes());
        }
        // 4-byte i32 types: i32, u32, char, enum, flags
        (
            WirType::I32
            | WirType::U32
            | WirType::Char
            | WirType::Enum { .. }
            | WirType::Flags { .. },
            WirInstr::I32Const(v),
        ) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // 8-byte i64 types: i64, u64
        (WirType::I64 | WirType::U64, WirInstr::I64Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // f32
        (WirType::F32, WirInstr::F32Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // f64
        (WirType::F64, WirInstr::F64Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        _ => return None,
    }
    Some(())
}

/// Maximum element count for `array.new_fixed`. Arrays larger than this are
/// rewritten to `array.new_default` + individual `array.set` instructions.
///
/// `array.new_fixed N` requires all N element values on the Wasm operand stack
/// simultaneously, which causes pathological JIT compilation times in Cranelift's
/// register allocator for large N (e.g. 8 000+ elements → minutes of JIT time).
/// The `array.set` form consumes each value immediately, keeping stack depth low.
pub(crate) const ARRAY_NEW_FIXED_LIMIT: usize = 256;

/// Split large `ArrayNewFixed` instructions into `ArrayNewDefault` + `ArraySet` sequences.
///
/// Walks all function bodies and rewrites any `ArrayNewFixed` with more than
/// [`ARRAY_NEW_FIXED_LIMIT`] elements. Uses a module-level counter for unique local names.
///
/// Unlike `promote_constant_arrays_to_data`, this deliberately skips global
/// initializers: the split form is a `Seq` of `DeclareLocal` / `LocalSet` /
/// `ArraySet` / `LocalGet`, none of which is a valid Wasm constant instruction,
/// so it cannot serve as an eager const-global init. A large *dynamic* array
/// literal is instead extracted to the `__initialize_module` function body by
/// lowering, where this pass reaches it; a large *const* array literal that
/// stays an eager global init keeps `array.new_fixed` (the JIT-time concern is a
/// runtime code path, not one-time module init).
pub(super) fn split_large_array_literals(module: &mut WirPackage) {
    let mut visitor = SplitLargeArrays { counter: 0 };
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                visitor.visit_instr(instr);
            }
        }
    }
}

struct SplitLargeArrays {
    counter: u32,
}

impl WirMutVisitor for SplitLargeArrays {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        // Recurse into children first (bottom-up).
        self.walk_instr(instr);

        // Check if THIS instruction is a large ArrayNewFixed that should be split.
        if let WirInstr::ArrayNewFixed { elements, .. } = instr
            && elements.len() > ARRAY_NEW_FIXED_LIMIT
        {
            rewrite_large_array_new_fixed(instr, &mut self.counter);
        }
    }
}

/// Rewrite a single `ArrayNewFixed` into `Seq([DeclareLocal, LocalSet(ArrayNewDefault), ArraySet*, LocalGet])`.
///
/// The resulting `Seq` is a value-producing sequence: the last instruction (`LocalGet`)
/// leaves the array reference on the stack, making this a drop-in replacement.
fn rewrite_large_array_new_fixed(instr: &mut WirInstr, counter: &mut u32) {
    let WirInstr::ArrayNewFixed { type_id, elements } = std::mem::replace(instr, WirInstr::Nop)
    else {
        return;
    };

    *counter += 1;
    let arr_local = format!("__wir_arr_init_{counter}");
    let len = i32::try_from(elements.len()).expect("array length fits i32");
    let raw_ref_type = WirType::Ref {
        type_id: type_id.clone(),
        nullable: true,
    };

    let mut seq = Vec::with_capacity(elements.len() + 3);
    seq.push(WirInstr::DeclareLocal {
        name: arr_local.clone(),
        ty: raw_ref_type.clone(),
    });
    seq.push(WirInstr::LocalSet {
        name: arr_local.clone(),
        value: Box::new(WirInstr::ArrayNewDefault {
            type_id: type_id.clone(),
            len: Box::new(WirInstr::I32Const(len)),
        }),
    });
    for (i, elem) in elements.into_iter().enumerate() {
        seq.push(WirInstr::ArraySet {
            type_id: type_id.clone(),
            array: Box::new(WirInstr::LocalGet {
                name: arr_local.clone(),
                result_ty: raw_ref_type.clone(),
            }),
            index: Box::new(WirInstr::I32Const(
                i32::try_from(i).expect("array index fits i32"),
            )),
            value: Box::new(elem),
        });
    }
    seq.push(WirInstr::LocalGet {
        name: arr_local,
        result_ty: raw_ref_type,
    });

    *instr = WirInstr::Seq(seq);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operand sizes the profitability comparison rests on, against the
    /// `T.const` encoding the emitter produces.
    #[test]
    fn const_operand_encoded_sizes() {
        assert_eq!(ConstOperand::I32(0).encoded_bytes(), 2);
        assert_eq!(ConstOperand::I32(63).encoded_bytes(), 2);
        assert_eq!(ConstOperand::I32(64).encoded_bytes(), 3);
        assert_eq!(ConstOperand::I32(-64).encoded_bytes(), 2);
        assert_eq!(ConstOperand::I32(-65).encoded_bytes(), 3);
        assert_eq!(ConstOperand::I32(8191).encoded_bytes(), 3);
        assert_eq!(ConstOperand::I32(8192).encoded_bytes(), 4);
        assert_eq!(ConstOperand::I32(i32::MAX).encoded_bytes(), 6);
        assert_eq!(ConstOperand::I64(i64::MAX).encoded_bytes(), 11);
        assert_eq!(ConstOperand::F32.encoded_bytes(), 5);
        assert_eq!(ConstOperand::F64.encoded_bytes(), 9);
    }

    fn operand_bytes(count: usize, operand: ConstOperand) -> usize {
        count * operand.encoded_bytes()
    }

    #[test]
    fn promotion_pays_only_when_packing_is_smaller() {
        let n = ARRAY_NEW_DATA_THRESHOLD;
        // `u8`: one packed byte against at least two inline.
        assert!(data_promotion_pays(
            n,
            1,
            operand_bytes(n, ConstOperand::I32(200))
        ));
        // `i32` of small values: four packed bytes against three inline.
        assert!(!data_promotion_pays(
            n,
            4,
            operand_bytes(n, ConstOperand::I32(182))
        ));
        // `i32` of values needing a four-byte LEB: five inline.
        assert!(data_promotion_pays(
            n,
            4,
            operand_bytes(n, ConstOperand::I32(3_000_000))
        ));
        // `f64`: eight packed bytes against nine inline.
        assert!(data_promotion_pays(
            n,
            8,
            operand_bytes(n, ConstOperand::F64)
        ));
        // `i64` of small values: eight packed bytes against three inline.
        assert!(!data_promotion_pays(
            n,
            8,
            operand_bytes(n, ConstOperand::I64(7))
        ));
    }

    #[test]
    fn below_the_threshold_never_promotes() {
        let n = ARRAY_NEW_DATA_THRESHOLD - 1;
        assert!(!data_promotion_pays(
            n,
            1,
            operand_bytes(n, ConstOperand::I32(200))
        ));
    }

    /// Past the fixed limit the alternative is the `array.new_default` +
    /// `array.set` build sequence, so packing wins even when it is larger.
    #[test]
    fn past_the_fixed_limit_promotion_always_pays() {
        let n = ARRAY_NEW_FIXED_LIMIT + 1;
        assert!(data_promotion_pays(
            n,
            8,
            operand_bytes(n, ConstOperand::I64(7))
        ));
    }
}
