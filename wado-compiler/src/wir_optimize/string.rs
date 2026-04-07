//! String optimization passes for WIR.
//!
//! - **Short string push simplification**: `String::push_str("short")` → `String::push_char` × N.

use crate::wir::{
    COMP_FEATURE_STRING_PUSH_CHAR, COMP_FEATURE_STRING_PUSH_STR, WirData, WirFuncId, WirInstr,
    WirPackage,
};
use crate::wir_visitor::WirMutVisitor;

/// Rewrite `String::push_str(buf, "short_constant")` calls into sequences of
/// `String::push_char(buf, ch)` calls when the constant string is ≤8 bytes.
///
/// This eliminates GC allocations for the temporary `String` struct and its
/// backing `array<u8>` that are created for each short constant string argument.
///
/// Pattern matched (WIR):
/// ```text
/// Call { func_id: <push_str>,
///   args: [receiver, StructNew { String,
///     fields: [RefAsNonNull(ArrayNewData { data_index, offset: 0, len: L }), I32Const(L)]
///   }]
/// }
/// ```
/// Rewritten to:
/// ```text
/// Call { func_id: <push_char>, args: [receiver, I32Const(byte0)] }
/// Call { func_id: <push_char>, args: [receiver, I32Const(byte1)] }
/// ...
/// ```
pub(super) fn simplify_short_string_pushes(module: &mut WirPackage) {
    // Find push_str and push_char function indices.
    let mut push_str_func_id: Option<WirFuncId> = None;
    let mut push_char_func_id: Option<WirFuncId> = None;

    for (i, f) in module.functions.iter().enumerate() {
        let idx = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
        if f.comp_features & COMP_FEATURE_STRING_PUSH_STR != 0 {
            push_str_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
        if f.comp_features & COMP_FEATURE_STRING_PUSH_CHAR != 0 {
            push_char_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
    }

    let (Some(push_str_id), Some(push_char_id)) = (push_str_func_id, push_char_func_id) else {
        return;
    };

    let data = &module.data;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            let mut visitor = SimplifyShortPushes {
                push_str_id: &push_str_id,
                push_char_id: &push_char_id,
                data,
            };
            visitor.visit_body(body);
        }
    }
}

struct SimplifyShortPushes<'a> {
    push_str_id: &'a WirFuncId,
    push_char_id: &'a WirFuncId,
    data: &'a [WirData],
}

impl WirMutVisitor for SimplifyShortPushes<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into children.
        self.walk_body(body);

        // Scan for String::push_str calls with short constant string args.
        let mut i = 0;
        while i < body.len() {
            if let Some(replacements) =
                try_rewrite_short_push_str(&body[i], self.push_str_id, self.push_char_id, self.data)
            {
                let n = replacements.len();
                body.splice(i..=i, replacements);
                i += n;
            } else {
                i += 1;
            }
        }
    }
}

/// Maximum byte length for short-constant string `push_str` optimization.
const MAX_SHORT_PUSH_STR_LEN: usize = 8;

/// Try to match and rewrite a single `String::push_str(buf, "short")` call.
/// Returns `None` if the instruction doesn't match the pattern.
/// Returns `Some(vec![push_char calls])` on success.
fn try_rewrite_short_push_str(
    instr: &WirInstr,
    push_str_id: &WirFuncId,
    push_char_id: &WirFuncId,
    data: &[WirData],
) -> Option<Vec<WirInstr>> {
    // Match: Call { func_id: push_str, args: [receiver, string_arg] }
    // Also match: Block { body: [Call { ... }] } (optimizer sometimes wraps in blocks)
    let call = match instr {
        WirInstr::Call { .. } => instr,
        WirInstr::Block { body, .. } | WirInstr::Seq(body) if body.len() == 1 => &body[0],
        _ => return None,
    };

    let WirInstr::Call { func_id, args } = call else {
        return None;
    };

    if func_id != push_str_id || args.len() != 2 {
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

    if str_len == 0 || str_len > MAX_SHORT_PUSH_STR_LEN {
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

    // Clone the receiver expression for each push_char call.
    let receiver = &args[0];

    let mut replacements = Vec::with_capacity(str_len);
    for &byte in bytes {
        replacements.push(WirInstr::Call {
            func_id: push_char_id.clone(),
            args: vec![receiver.clone(), WirInstr::I32Const(i32::from(byte))],
        });
    }

    Some(replacements)
}
