//! String optimization passes for WIR.
//!
//! - **Short string append simplification**: `String::append("short")` → `String::append_char` × N.

use crate::wir::{
    COMP_FEATURE_STRING_APPEND, COMP_FEATURE_STRING_APPEND_CHAR, WirData, WirFuncId, WirInstr,
    WirModule,
};
use crate::wir_visitor::WirMutVisitor;

/// Rewrite `String::append(buf, "short_constant")` calls into sequences of
/// `String::append_char(buf, ch)` calls when the constant string is ≤8 bytes.
///
/// This eliminates GC allocations for the temporary `String` struct and its
/// backing `array<u8>` that are created for each short constant string argument.
///
/// Pattern matched (WIR):
/// ```text
/// Call { func_id: <string_append>,
///   args: [receiver, StructNew { String,
///     fields: [RefAsNonNull(ArrayNewData { data_index, offset: 0, len: L }), I32Const(L)]
///   }]
/// }
/// ```
/// Rewritten to:
/// ```text
/// Call { func_id: <string_append_char>, args: [receiver, I32Const(byte0)] }
/// Call { func_id: <string_append_char>, args: [receiver, I32Const(byte1)] }
/// ...
/// ```
pub(super) fn simplify_short_string_appends(module: &mut WirModule) {
    // Find string_append and string_append_char function indices.
    let mut append_func_id: Option<WirFuncId> = None;
    let mut append_char_func_id: Option<WirFuncId> = None;

    for (i, f) in module.functions.iter().enumerate() {
        let idx = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
        if f.comp_features & COMP_FEATURE_STRING_APPEND != 0 {
            append_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
        if f.comp_features & COMP_FEATURE_STRING_APPEND_CHAR != 0 {
            append_char_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
    }

    let (Some(append_id), Some(append_char_id)) = (append_func_id, append_char_func_id) else {
        return;
    };

    let data = &module.data;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            let mut visitor = SimplifyShortAppends {
                append_id: &append_id,
                append_char_id: &append_char_id,
                data,
            };
            visitor.visit_body(body);
        }
    }
}

struct SimplifyShortAppends<'a> {
    append_id: &'a WirFuncId,
    append_char_id: &'a WirFuncId,
    data: &'a [WirData],
}

impl WirMutVisitor for SimplifyShortAppends<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into children.
        self.walk_body(body);

        // Scan for String::append calls with short constant string args.
        let mut i = 0;
        while i < body.len() {
            if let Some(replacements) = try_rewrite_short_string_append(
                &body[i],
                self.append_id,
                self.append_char_id,
                self.data,
            ) {
                let n = replacements.len();
                body.splice(i..=i, replacements);
                i += n;
            } else {
                i += 1;
            }
        }
    }
}

/// Maximum byte length for short-constant string append optimization.
const MAX_SHORT_STRING_APPEND_LEN: usize = 8;

/// Try to match and rewrite a single `String::append(buf, "short")` call.
/// Returns `None` if the instruction doesn't match the pattern.
/// Returns `Some(vec![append_char calls])` on success.
fn try_rewrite_short_string_append(
    instr: &WirInstr,
    append_id: &WirFuncId,
    append_char_id: &WirFuncId,
    data: &[WirData],
) -> Option<Vec<WirInstr>> {
    // Match: Call { func_id: string_append, args: [receiver, string_arg] }
    // Also match: Block { body: [Call { ... }] } (optimizer sometimes wraps in blocks)
    let call = match instr {
        WirInstr::Call { .. } => instr,
        WirInstr::Block { body, .. } | WirInstr::Seq(body) if body.len() == 1 => &body[0],
        _ => return None,
    };

    let WirInstr::Call { func_id, args } = call else {
        return None;
    };

    if func_id != append_id || args.len() != 2 {
        return None;
    }

    // Match the second arg: StructNew { fields: [RefAsNonNull(ArrayNewData { ... }), I32Const(len)] }
    let WirInstr::StructNew { fields, .. } = &args[1] else {
        return None;
    };

    if fields.len() != 2 {
        return None;
    }

    // Extract ArrayNewData from RefAsNonNull wrapper.
    let array_new_data = match &fields[0] {
        WirInstr::RefAsNonNull(inner) => inner.as_ref(),
        other => other,
    };

    let WirInstr::ArrayNewData {
        data_index,
        offset,
        len,
        ..
    } = array_new_data
    else {
        return None;
    };

    // Verify offset is 0 and len is a small constant.
    let WirInstr::I32Const(0) = offset.as_ref() else {
        return None;
    };
    let WirInstr::I32Const(str_len_i32) = len.as_ref() else {
        return None;
    };
    let str_len = usize::try_from(*str_len_i32).ok()?;

    if str_len == 0 || str_len > MAX_SHORT_STRING_APPEND_LEN {
        return None;
    }

    // Verify the used field matches.
    let WirInstr::I32Const(used) = &fields[1] else {
        return None;
    };
    if *used != *str_len_i32 {
        return None;
    }

    // Get the actual bytes from the data segment.
    let seg = data.get(*data_index as usize)?;
    if seg.bytes.len() < str_len {
        return None;
    }
    let bytes = &seg.bytes[..str_len];

    // Clone the receiver expression for each append_char call.
    let receiver = &args[0];

    let mut replacements = Vec::with_capacity(str_len);
    for &byte in bytes {
        replacements.push(WirInstr::Call {
            func_id: append_char_id.clone(),
            args: vec![receiver.clone(), WirInstr::I32Const(i32::from(byte))],
        });
    }

    Some(replacements)
}
