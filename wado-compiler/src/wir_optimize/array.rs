//! Array optimization passes for WIR.
//!
//! - **Constant array data promotion**: `ArrayNewFixed` of constants → `ArrayNewData`.
//! - **Large array literal splitting**: `array.new_fixed` (>= threshold) → `array.new_default` + sets.
//! - **Array push collapse**: inlined `Array::push` sequences → `ArrayNewFixed`.

use crate::hashmap::IndexSet;
use crate::wir::{
    COMP_FEATURE_ARRAY_PUSH, WirData, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId,
};
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

/// Collapse inlined `Array::push` sequences back to `ArrayNewFixed`.
///
/// After the `SequenceLiteralBuilder` trait path is inlined, array literals like
/// `[10, 20, 30]` become:
///
/// ```text
/// LocalSet { name: X, value: StructNew { ... ArrayNewDefault(N) ... I32Const(0) } }
/// Block { Call { Array::push(receiver, v0) } }
/// Block { Call { Array::push(receiver, v1) } }
/// ...
/// ```
///
/// This pass recognizes that pattern and rewrites it to use `ArrayNewFixed`
/// (replacing `ArrayNewDefault` and removing the push calls), which is then
/// eligible for `promote_constant_arrays_to_data` and `split_large_array_literals`.
pub(super) fn collapse_array_push_sequences(module: &mut WirPackage) {
    // Build set of function indices that have COMP_FEATURE_ARRAY_PUSH.
    let push_func_indices: IndexSet<u32> = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, f)| f.comp_features & COMP_FEATURE_ARRAY_PUSH != 0)
        .map(|(i, _)| crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap())
        .collect();

    if push_func_indices.is_empty() {
        return;
    }

    // Build map: type index → is Array<T> struct (has generic_origin.base_name == "Array").
    let array_struct_types: IndexSet<u32> = module
        .types
        .iter()
        .enumerate()
        .filter_map(|(i, td)| {
            if let WirTypeDef::Struct(s) = td
                && s.generic_origin
                    .as_ref()
                    .is_some_and(|g| g.base_name == "Array")
            {
                Some(u32::try_from(i).unwrap())
            } else {
                None
            }
        })
        .collect();

    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            let mut visitor = CollapsePushes {
                push_func_indices: &push_func_indices,
                array_struct_types: &array_struct_types,
            };
            visitor.visit_body(body);
        }
    }
}

/// Describes how the Array<T> is accessed from the local variable.
#[derive(Debug, Clone)]
enum ArrayAccessPath {
    /// The local IS the Array<T> struct directly.
    Direct,
    /// The Array<T> is a field of the local's struct type.
    Field { outer_type_idx: u32 },
}

/// Information about a detected Array<T> initialization via `SequenceLiteralBuilder`.
struct ArrayInitInfo {
    /// Name of the local variable holding the struct.
    local_name: String,
    /// WIR type ID of the raw Wasm array type (e.g., `builtin::array<i32>`).
    raw_array_type_id: WirTypeId,
    /// Expected number of pushes (from `ArrayNewDefault` capacity).
    capacity: usize,
    /// How to access the Array<T> from the local.
    access_path: ArrayAccessPath,
    /// Index of the `I32Const(0)` field (the `used` counter) within the Array struct fields.
    /// Needed to rewrite it to `I32Const(N)`.
    used_field_index: usize,
}

struct CollapsePushes<'a> {
    push_func_indices: &'a IndexSet<u32>,
    array_struct_types: &'a IndexSet<u32>,
}

impl WirMutVisitor for CollapsePushes<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into all children.
        self.walk_body(body);

        // Now scan the flat body for init + push patterns.
        let mut i = 0;
        while i < body.len() {
            if let Some(init_info) = try_match_array_init(&body[i], self.array_struct_types) {
                let n = init_info.capacity;
                // Check if the next instructions are matching push calls.
                // Each push may be a single instruction (Block wrapping LocalSet+Call)
                // or multiple flat instructions (LocalSet* + Call) from block flattening.
                if n > 0
                    && let Some((values, consumed)) = try_match_push_sequence(
                        &body[i + 1..],
                        n,
                        &init_info,
                        self.push_func_indices,
                    )
                {
                    // Rewrite: replace ArrayNewDefault with ArrayNewFixed in the init.
                    rewrite_init_to_fixed(&mut body[i], &init_info, values);
                    // Remove the consumed push instructions.
                    body.drain(i + 1..i + 1 + consumed);
                    // Continue from the next instruction after the rewritten init.
                    i += 1;
                    continue;
                }
            }
            i += 1;
        }
    }
}

/// Try to match a `LocalSet` that initializes an Array<T> via `SequenceLiteralBuilder`.
///
/// Matches patterns like:
/// ```text
/// LocalSet { name: X, value: StructNew { type_id: OUTER,
///   fields: [RefAsNonNull(StructNew { type_id: ARRAY,
///     fields: [RefAsNonNull(ArrayNewDefault { type_id: RAW, len: I32Const(N) }), I32Const(0)]
///   })]
/// }}
/// ```
/// or the direct Array<T> case:
/// ```text
/// LocalSet { name: X, value: StructNew { type_id: ARRAY,
///   fields: [RefAsNonNull(ArrayNewDefault { type_id: RAW, len: I32Const(N) }), I32Const(0)]
/// }}
/// ```
fn try_match_array_init(
    instr: &WirInstr,
    array_struct_types: &IndexSet<u32>,
) -> Option<ArrayInitInfo> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };

    let WirInstr::StructNew { type_id, fields } = value.as_ref() else {
        return None;
    };

    // Case 1: Direct Array<T> init (LocalSet { name, StructNew { Array<T>, [RefAsNonNull(ArrayNewDefault), I32Const(0)] } })
    if array_struct_types.contains(&type_id.index()) {
        return try_extract_array_new_default(fields, name.clone(), ArrayAccessPath::Direct);
    }

    // Case 2: Wrapper struct with Array<T> field
    // Look through fields for a RefAsNonNull(StructNew { Array<T>, ... })
    for (field_idx, field) in fields.iter().enumerate() {
        let inner_struct_new = match field {
            WirInstr::RefAsNonNull(inner) => inner.as_ref(),
            _ => field,
        };

        if let WirInstr::StructNew {
            type_id: inner_type_id,
            fields: inner_fields,
        } = inner_struct_new
            && array_struct_types.contains(&inner_type_id.index())
        {
            // Find the field name for this index from the outer struct.
            // We need to match against the access path later, so we need
            // the field name. Since WIR doesn't store field names in StructNew,
            // we need to figure it out differently. Actually, looking at the
            // WIR debug output, the `StructGet` uses the field name, and
            // the StructNew field order matches the struct definition order.
            // We'll use the field index to look up the name later in matching.
            // For now, record the outer type and field index.
            let _ = field_idx; // suppress unused warning
            return try_extract_array_new_default(
                inner_fields,
                name.clone(),
                ArrayAccessPath::Field {
                    outer_type_idx: type_id.index(),
                },
            );
        }
    }

    None
}

/// Try to extract `ArrayNewDefault` info from Array<T> struct fields.
/// Expected: `[RefAsNonNull(ArrayNewDefault { type_id, len: I32Const(N) }), I32Const(0)]`
fn try_extract_array_new_default(
    fields: &[WirInstr],
    local_name: String,
    access_path: ArrayAccessPath,
) -> Option<ArrayInitInfo> {
    if fields.len() != 2 {
        return None;
    }

    // First field: ArrayNewDefault or RefAsNonNull(ArrayNewDefault)
    let array_new_default = match &fields[0] {
        WirInstr::RefAsNonNull(inner) => inner.as_ref(),
        other => other,
    };
    let WirInstr::ArrayNewDefault { type_id, len } = array_new_default else {
        return None;
    };
    let WirInstr::I32Const(capacity) = len.as_ref() else {
        return None;
    };
    let capacity = usize::try_from(*capacity).ok()?;

    // Second field: I32Const(0) (the `used` counter)
    let WirInstr::I32Const(0) = &fields[1] else {
        return None;
    };

    Some(ArrayInitInfo {
        local_name,
        raw_array_type_id: type_id.clone(),
        capacity,
        access_path,
        used_field_index: 1,
    })
}

/// Try to match N push operations starting from the given instruction slice.
/// Each push may be either:
/// - A single `Block` instruction wrapping `LocalSet* + Call` (from inlined labeled blocks)
/// - A sequence of flat `LocalSet* + Call` instructions (from flattened blocks)
///
/// Returns the extracted element values and the total number of instructions consumed,
/// or `None` if the pattern doesn't match.
fn try_match_push_sequence(
    instrs: &[WirInstr],
    expected_count: usize,
    init_info: &ArrayInitInfo,
    push_func_indices: &IndexSet<u32>,
) -> Option<(Vec<WirInstr>, usize)> {
    let mut values = Vec::with_capacity(expected_count);
    let mut consumed = 0;

    while values.len() < expected_count && consumed < instrs.len() {
        let instr = &instrs[consumed];

        // Try pattern 1: Block wrapping LocalSet* + Call (from inlined labeled blocks)
        let (call, aliases, value_bindings) = extract_call_from_block(instr);
        if let WirInstr::Call { func_id, args } = call
            && push_func_indices.contains(&func_id.index())
            && args.len() == 2
            && receiver_matches_with_aliases(&args[0], init_info, &aliases)
        {
            let element = resolve_value_binding(&args[1], &value_bindings, &aliases);
            values.push(element);
            consumed += 1;
            continue;
        }

        // Try pattern 2: Flat LocalSet* + Call sequence (from flattened blocks)
        // Collect leading LocalSet instructions, then expect a matching Call.
        let mut flat_aliases = Vec::new();
        let mut flat_value_bindings = Vec::new();
        let mut j = consumed;
        while j < instrs.len() {
            if let WirInstr::LocalSet { name, value } = &instrs[j] {
                if let WirInstr::LocalGet { name: src_name, .. } = value.as_ref() {
                    flat_aliases.push((name.clone(), src_name.clone()));
                } else {
                    flat_value_bindings.push((name.clone(), *value.clone()));
                }
                j += 1;
            } else {
                break;
            }
        }
        // We must have consumed at least one LocalSet (otherwise pattern 1 would match)
        // and the next instruction must be a matching Call.
        if j > consumed
            && j < instrs.len()
            && let WirInstr::Call { func_id, args } = &instrs[j]
            && push_func_indices.contains(&func_id.index())
            && args.len() == 2
            && receiver_matches_with_aliases(&args[0], init_info, &flat_aliases)
        {
            let element = resolve_value_binding(&args[1], &flat_value_bindings, &flat_aliases);
            values.push(element);
            consumed = j + 1;
            continue;
        }

        // Neither pattern matched
        return None;
    }

    if values.len() == expected_count {
        Some((values, consumed))
    } else {
        None
    }
}

/// Resolve a value through value bindings and aliases from the enclosing block.
/// If the instruction is a `LocalGet` that refers to a value binding, return the
/// bound value. If it refers to an alias, resolve through the alias chain.
/// Otherwise return a clone of the instruction as-is.
fn resolve_value_binding(
    instr: &WirInstr,
    value_bindings: &[(String, WirInstr)],
    aliases: &[(String, String)],
) -> WirInstr {
    if let WirInstr::LocalGet {
        name, result_ty, ..
    } = instr
    {
        for (binding_name, binding_value) in value_bindings {
            if binding_name == name {
                return binding_value.clone();
            }
        }
        // Also resolve through aliases (e.g., inlined parameter that aliases a caller local)
        let resolved = resolve_alias(name, aliases);
        if resolved != name {
            return WirInstr::LocalGet {
                name: resolved.to_string(),
                result_ty: result_ty.clone(),
            };
        }
    }
    instr.clone()
}

/// Extract a Call instruction from inside a Block, along with local aliases
/// and value bindings from preceding `LocalSet` instructions.
///
/// After inlining, a `push_literal` call often expands to:
/// ```text
/// Block { body: [
///   LocalSet { name: "__local_7", value: LocalGet { name: "__local_0" } },
///   Call { func_id: push, args: [LocalGet { name: "__local_7" }, value] }
/// ] }
/// ```
///
/// For non-scalar elements (e.g., `String`), the element value is materialized
/// in a separate `LocalSet`:
/// ```text
/// Block { body: [
///   LocalSet { name: "__local_4", value: LocalGet { name: "__local_0" } },
///   LocalSet { name: "__local_5", value: StructNew { String, ... } },
///   Call { func_id: push, args: [LocalGet("__local_4"), LocalGet("__local_5")] }
/// ] }
/// ```
///
/// Returns the Call instruction, a list of (`alias_name`, `original_name`) pairs,
/// and a list of (`binding_name`, `value_expr`) pairs for non-alias bindings.
fn extract_call_from_block(
    instr: &WirInstr,
) -> (&WirInstr, Vec<(String, String)>, Vec<(String, WirInstr)>) {
    // Accept both Block (from inlined labeled blocks) and Seq (from flattened blocks).
    let body = match instr {
        WirInstr::Block {
            body, result: None, ..
        }
        | WirInstr::Seq(body) => body,
        _ => return (instr, Vec::new(), Vec::new()),
    };

    if body.is_empty() {
        return (instr, Vec::new(), Vec::new());
    }

    // The last instruction should be the Call.
    let call = body.last().unwrap();

    // Preceding instructions should be LocalSet: either aliases (LocalGet) or value bindings.
    let mut aliases = Vec::new();
    let mut value_bindings = Vec::new();
    for preceding in &body[..body.len() - 1] {
        if let WirInstr::LocalSet { name, value } = preceding {
            if let WirInstr::LocalGet { name: src_name, .. } = value.as_ref() {
                aliases.push((name.clone(), src_name.clone()));
            } else {
                value_bindings.push((name.clone(), *value.clone()));
            }
        } else {
            // Non-LocalSet instruction before the call — bail out.
            return (instr, Vec::new(), Vec::new());
        }
    }

    (call, aliases, value_bindings)
}

/// Check if a receiver expression matches the expected access path for the Array<T>,
/// resolving local aliases from inline expansion.
fn receiver_matches_with_aliases(
    receiver: &WirInstr,
    init_info: &ArrayInitInfo,
    aliases: &[(String, String)],
) -> bool {
    match &init_info.access_path {
        ArrayAccessPath::Direct => {
            // Receiver should be LocalGet { name } where name resolves to init_info.local_name
            if let WirInstr::LocalGet { name, .. } = receiver {
                resolve_alias(name, aliases) == init_info.local_name
            } else {
                false
            }
        }
        ArrayAccessPath::Field { outer_type_idx } => {
            // Receiver should be StructGet { type_id, expr: LocalGet { name } }
            if let WirInstr::StructGet { type_id, expr, .. } = receiver {
                if type_id.index() != *outer_type_idx {
                    return false;
                }
                if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                    resolve_alias(name, aliases) == init_info.local_name
                } else {
                    false
                }
            } else {
                false
            }
        }
    }
}

/// Resolve a local name through a chain of aliases.
/// If `name` appears as an alias target, return the source name (recursively).
fn resolve_alias<'a>(name: &'a str, aliases: &'a [(String, String)]) -> &'a str {
    for (alias_name, original_name) in aliases {
        if alias_name == name {
            return resolve_alias(original_name, aliases);
        }
    }
    name
}

/// Rewrite the init instruction to use `ArrayNewFixed` instead of `ArrayNewDefault`,
/// and update the `used` counter from 0 to N.
fn rewrite_init_to_fixed(instr: &mut WirInstr, init_info: &ArrayInitInfo, values: Vec<WirInstr>) {
    let n = i32::try_from(values.len()).unwrap_or(0);

    // Navigate into the instruction tree to find and replace ArrayNewDefault.
    let WirInstr::LocalSet { value, .. } = instr else {
        return;
    };

    let array_fields = match value.as_mut() {
        // Direct Array<T>
        WirInstr::StructNew { fields, .. } if init_info.access_path.is_direct() => fields,
        // Wrapper struct containing Array<T>
        WirInstr::StructNew { fields, .. } => {
            // Find the Array<T> StructNew inside the wrapper fields.
            let Some(inner_fields) = find_inner_array_fields(fields) else {
                return;
            };
            inner_fields
        }
        _ => return,
    };

    // Replace fields[0]: ArrayNewDefault → ArrayNewFixed (with or without RefAsNonNull)
    if let Some(first) = array_fields.first_mut() {
        let new_fixed = WirInstr::ArrayNewFixed {
            type_id: init_info.raw_array_type_id.clone(),
            elements: values,
        };
        match first {
            WirInstr::RefAsNonNull(inner) => **inner = new_fixed,
            _ => *first = new_fixed,
        }
    }

    // Replace fields[used_field_index]: I32Const(0) → I32Const(N)
    if let Some(used_field) = array_fields.get_mut(init_info.used_field_index) {
        *used_field = WirInstr::I32Const(n);
    }
}

impl ArrayAccessPath {
    fn is_direct(&self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// Find the inner Array<T> fields within a wrapper struct's fields.
/// Looks for `StructNew { fields }` (with or without `RefAsNonNull` wrapper).
fn find_inner_array_fields(outer_fields: &mut [WirInstr]) -> Option<&mut Vec<WirInstr>> {
    for field in outer_fields.iter_mut() {
        let struct_new = match field {
            WirInstr::RefAsNonNull(inner) => inner.as_mut(),
            other => other,
        };
        if let WirInstr::StructNew { fields, .. } = struct_new
            && fields.len() == 2
            && is_array_new_default(&fields[0])
            && matches!(&fields[1], WirInstr::I32Const(0))
        {
            return Some(fields);
        }
    }
    None
}

/// Check if an instruction is `ArrayNewDefault` (with or without `RefAsNonNull` wrapper).
fn is_array_new_default(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::ArrayNewDefault { .. } => true,
        WirInstr::RefAsNonNull(inner) => matches!(inner.as_ref(), WirInstr::ArrayNewDefault { .. }),
        _ => false,
    }
}
