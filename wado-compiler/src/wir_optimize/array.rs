//! Array optimization passes for WIR.
//!
//! - **Constant array data promotion**: `ArrayNewFixed` of constants → `ArrayNewData`.
//! - **Large array literal splitting**: `array.new_fixed` (>= threshold) → `array.new_default` + sets.
//!
//! Array literals reach WIR already as `ArrayNewFixed`: the NIR
//! `optimize::array_literal` pass materializes `NirExprKind::ArrayLiteral` from
//! the `SequenceLiteralBuilder` push sequence, and `wir_build` lowers it
//! directly. The former WIR-level `collapse_array_push_sequences` that
//! reconstructed `ArrayNewFixed` from inlined `Array::push` chains is therefore
//! retired; the passes below consume the `ArrayNewFixed` it used to produce.

use crate::wir::{WirData, WirInstr, WirPackage, WirType, WirTypeDef};
use crate::wir_visitor::WirMutVisitor;

/// Minimum element count to trigger `array.new_data` promotion. Arrays with
/// fewer constant elements keep using `array.new_fixed`.
const ARRAY_NEW_DATA_THRESHOLD: usize = 128;

/// Promote constant primitive `ArrayNewFixed` to `ArrayNewData`.
///
/// When all elements of an `ArrayNewFixed` are compile-time constants of a
/// primitive type, packs the values into a passive data segment and replaces
/// the instruction with `ArrayNewData`. This reduces Wasm binary size and
/// initialization overhead compared to pushing N constants + `array.new_fixed`.
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

    // Also check global initializers (e.g., `global ITEMS: Array<i32> = [1,2,3]`).
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
            {
                let data_index = u32::try_from(self.data.len()).expect("too many data segments");
                let len = i32::try_from(elements.len()).unwrap_or(0);
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
const ARRAY_NEW_FIXED_LIMIT: usize = 256;

/// Split large `ArrayNewFixed` instructions into `ArrayNewDefault` + `ArraySet` sequences.
///
/// Walks all function bodies and rewrites any `ArrayNewFixed` with more than
/// [`ARRAY_NEW_FIXED_LIMIT`] elements. Uses a module-level counter for unique local names.
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
    let len = i32::try_from(elements.len()).unwrap_or(0);
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
            index: Box::new(WirInstr::I32Const(i32::try_from(i).unwrap_or(0))),
            value: Box::new(elem),
        });
    }
    seq.push(WirInstr::LocalGet {
        name: arr_local,
        result_ty: raw_ref_type,
    });

    *instr = WirInstr::Seq(seq);
}
