//! Peephole optimization pass for WIR.
//!
//! Per-function pass that applies local rewrites:
//! - Constant folding on integer comparisons
//! - Dead `If` elimination (constant condition)
//! - Redundant `ValueCopy` elision
//! - Copy-used-only-for-field-reads elision
//! - Multi-value struct elision (`MultiValueStructNew` + `StructGet` → `MultiValueLocalBind`)

use crate::wir::{WirInstr, WirTypeDef};

/// Recursively optimize a list of instructions.
///
/// First descends into nested instruction bodies (Block, Loop, If, Seq),
/// then applies flat-level optimizations on the current list.
pub(super) fn run_peephole(instrs: &mut Vec<WirInstr>, types: &[WirTypeDef]) {
    for instr in instrs.iter_mut() {
        optimize_nested(instr, types);
    }
    fold_constant_comparisons(instrs);
    elide_redundant_value_copies(instrs);
    elide_copy_used_only_for_field_reads(instrs);
    elide_multi_value_structs(instrs, types);
}

/// Recurse into nested instruction bodies and eliminate dead branches.
fn optimize_nested(instr: &mut WirInstr, types: &[WirTypeDef]) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            run_peephole(body, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            result,
        } => {
            run_peephole(then_body, types);
            if let Some(eb) = else_body {
                run_peephole(eb, types);
            }
            // Dead If elimination: replace with surviving branch when condition is constant
            if let Some(const_val) = try_fold_wir_to_bool(condition) {
                if const_val {
                    let then_instrs = std::mem::take(then_body);
                    *instr = WirInstr::Block {
                        label: None,
                        result: result.clone(),
                        body: then_instrs,
                    };
                } else if let Some(eb) = else_body {
                    let else_instrs = std::mem::take(eb);
                    *instr = WirInstr::Block {
                        label: None,
                        result: result.clone(),
                        body: else_instrs,
                    };
                } else {
                    *instr = WirInstr::Block {
                        label: None,
                        result: None,
                        body: vec![WirInstr::Nop],
                    };
                }
            }
        }
        WirInstr::Seq(body) => {
            run_peephole(body, types);
        }
        WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. } => {
            optimize_nested(value, types);
        }
        WirInstr::ValueCopy { expr, .. } => {
            optimize_nested(expr, types);
        }
        _ => {}
    }
}

/// Try to evaluate a WIR condition to a boolean constant.
fn try_fold_wir_to_bool(instr: &WirInstr) -> Option<bool> {
    match instr {
        WirInstr::I32Const(v) => Some(*v != 0),
        _ => None,
    }
}

/// Recursively fold constant integer comparisons to `I32Const`.
fn fold_constant_comparisons(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        fold_constant_comparisons_in_instr(instr);
    }
}

fn fold_constant_comparisons_in_instr(instr: &mut WirInstr) {
    // Recurse into children first (bottom-up folding)
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            fold_constant_comparisons(body);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            fold_constant_comparisons_in_instr(condition);
            fold_constant_comparisons(then_body);
            if let Some(eb) = else_body {
                fold_constant_comparisons(eb);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                fold_constant_comparisons_in_instr(child);
            });
        }
    }

    // Then try to fold this instruction
    let result = match instr {
        WirInstr::I32GeS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv >= *rv)),
            _ => None,
        },
        WirInstr::I32GeU(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => {
                Some(i32::from(lv.cast_unsigned() >= rv.cast_unsigned()))
            }
            _ => None,
        },
        WirInstr::I32LtS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv < *rv)),
            _ => None,
        },
        WirInstr::I32GtS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv > *rv)),
            _ => None,
        },
        WirInstr::I32Eq(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv == *rv)),
            _ => None,
        },
        WirInstr::I32Ne(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv != *rv)),
            _ => None,
        },
        WirInstr::I32LeS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv <= *rv)),
            _ => None,
        },
        _ => None,
    };

    if let Some(val) = result {
        *instr = WirInstr::I32Const(val);
    }
}

/// Remove unnecessary `ValueCopy` wrappers from `LocalSet` instructions.
///
/// A `ValueCopy` deep-copies a GC value to maintain Wado's value semantics.
/// This copy is unnecessary when the source expression provably produces a
/// **fresh** value — one with no pre-existing aliases. Fresh values include
/// GC constructors (`StructNew`, `ArrayNew*`), function call return values,
/// and block expressions whose result is itself fresh.
fn elide_redundant_value_copies(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        let WirInstr::LocalSet { value, .. } = instr else {
            continue;
        };
        let is_fresh = if let WirInstr::ValueCopy { expr, .. } = value.as_ref() {
            is_fresh_wir_value(expr)
        } else {
            false
        };
        if is_fresh {
            let old = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if let WirInstr::ValueCopy { expr, .. } = old {
                *value = expr;
            }
        }
    }
}

/// Elide `value_copy T(expr)` when the copy result is used exclusively for
/// struct field reads (`StructGet`).
///
/// This is safe because:
/// - Reading a primitive field from the original or a copy yields the same value.
/// - Reading a complex (GC) field is safe when the extracted value itself goes
///   through its own `value_copy` (which provides the necessary isolation).
/// - If `t` were used for any mutation (e.g. `StructSet`, `Call` taking `t` by
///   reference), the check would fail and the copy would be preserved.
///
/// The canonical case is struct destructuring in a for-of loop:
/// ```text
/// __pattern_temp_1 = value_copy Point(ref.as_non_null(__pattern_temp_0))
/// x = __pattern_temp_1.x    // StructGet → safe
/// y = __pattern_temp_1.y    // StructGet → safe
/// ```
fn elide_copy_used_only_for_field_reads(instrs: &mut [WirInstr]) {
    for i in 0..instrs.len() {
        let var_name = match &instrs[i] {
            WirInstr::LocalSet { name, value }
                if matches!(value.as_ref(), WirInstr::ValueCopy { .. }) =>
            {
                name.clone()
            }
            _ => continue,
        };
        let all_reads_only = instrs[i + 1..]
            .iter()
            .all(|instr| uses_of_var_are_field_reads_only(instr, &var_name));
        if all_reads_only && let WirInstr::LocalSet { value, .. } = &mut instrs[i] {
            let old = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if let WirInstr::ValueCopy { expr, .. } = old {
                *value.as_mut() = *expr;
            }
        }
    }
}

/// Returns `true` if every use of `var_name` in `instr` is a struct field read,
/// or if `var_name` does not appear in `instr` at all.
fn uses_of_var_are_field_reads_only(instr: &WirInstr, var_name: &str) -> bool {
    match instr {
        WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. } => {
            match value.as_ref() {
                // `x = t.field` — direct field read.
                WirInstr::StructGet { expr, .. } => {
                    if let WirInstr::LocalGet { name } = expr.as_ref()
                        && name == var_name
                    {
                        return true;
                    }
                    !instr_contains_local_get(value, var_name)
                }
                // `x = value_copy T(t.field)` — field read wrapped in value_copy.
                WirInstr::ValueCopy { expr, .. } => {
                    if let WirInstr::StructGet { expr: inner, .. } = expr.as_ref()
                        && let WirInstr::LocalGet { name } = inner.as_ref()
                        && name == var_name
                    {
                        return true;
                    }
                    !instr_contains_local_get(value, var_name)
                }
                _ => !instr_contains_local_get(value, var_name),
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => body
            .iter()
            .all(|i| uses_of_var_are_field_reads_only(i, var_name)),
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            !instr_contains_local_get(condition, var_name)
                && then_body
                    .iter()
                    .all(|i| uses_of_var_are_field_reads_only(i, var_name))
                && else_body.as_ref().is_none_or(|eb| {
                    eb.iter()
                        .all(|i| uses_of_var_are_field_reads_only(i, var_name))
                })
        }
        _ => !instr_contains_local_get(instr, var_name),
    }
}

/// Returns `true` if `var_name` appears as a `LocalGet` anywhere in `instr`.
fn instr_contains_local_get(instr: &WirInstr, var_name: &str) -> bool {
    if let WirInstr::LocalGet { name } = instr {
        return name == var_name;
    }
    let mut found = false;
    instr.for_each_child(&mut |child| {
        if instr_contains_local_get(child, var_name) {
            found = true;
        }
    });
    found
}

/// Check if a WIR instruction provably produces a fresh value (no aliases).
fn is_fresh_wir_value(instr: &WirInstr) -> bool {
    match instr {
        // GC constructors create fresh values.
        WirInstr::StructNew { .. }
        | WirInstr::ArrayNew { .. }
        | WirInstr::ArrayNewDefault { .. }
        | WirInstr::ArrayNewData { .. }
        | WirInstr::ArrayNewFixed { .. } => true,

        // Function calls return fresh values (callee constructs them).
        WirInstr::Call { .. } | WirInstr::CallRef { .. } => true,

        // RefAsNonNull wrapping a fresh value is still fresh.
        WirInstr::RefAsNonNull(inner) => is_fresh_wir_value(inner),

        // Block with a result: trace through to find the result value.
        WirInstr::Block {
            body,
            result: Some(_),
            ..
        } => block_result_is_fresh(body),

        // Seq: check the effective result instruction.
        WirInstr::Seq(body) => find_seq_result(body).is_some_and(is_fresh_wir_value),

        _ => false,
    }
}

/// Determine if a block's result value is fresh.
///
/// The block's result is produced by either:
/// - A trailing `Seq([..., result_expr, Br(0)])` — the instruction before `Br`
/// - A trailing `Seq([..., LocalGet(x), Br(0)])` — trace back to find `x`'s value
/// - The last instruction in the body (fall-through)
fn block_result_is_fresh(body: &[WirInstr]) -> bool {
    let result_instr = match body.last() {
        Some(WirInstr::Seq(seq)) => find_seq_result(seq),
        Some(instr) => Some(instr),
        None => return false,
    };
    match result_instr {
        Some(WirInstr::LocalGet { name }) => {
            // Trace back: find what this local was set to within the block.
            find_local_set_value(body, name).is_some_and(is_fresh_wir_value)
        }
        Some(instr) => is_fresh_wir_value(instr),
        None => false,
    }
}

/// Find the effective result instruction of a `Seq`.
///
/// In a `Seq` ending with `Br`, the result is the instruction just before `Br`
/// (it leaves a value on the stack that `Br` carries to the enclosing block).
/// Otherwise, the last instruction is the result.
fn find_seq_result(body: &[WirInstr]) -> Option<&WirInstr> {
    if body.len() >= 2 && matches!(body.last(), Some(WirInstr::Br { .. })) {
        Some(&body[body.len() - 2])
    } else {
        body.last()
    }
}

/// Scan backward through a block body to find the value assigned to a local.
fn find_local_set_value<'a>(body: &'a [WirInstr], target: &str) -> Option<&'a WirInstr> {
    for instr in body.iter().rev() {
        if let WirInstr::LocalSet { name, value } = instr
            && name == target
        {
            return Some(value.as_ref());
        }
    }
    None
}

/// Multi-value tuple elision pass.
///
/// Matches the pattern generated by `translate_let_pattern` for multi-value
/// builtins (`i64.add128`, `i64.sub128`, `i64.mul_wide_u`, `i64.mul_wide_s`):
///
/// ```text
/// DeclareLocal { name: T, .. }                                              // optional
/// LocalSet { name: T, value: MultiValueStructNew { type_id, instr } }
/// LocalSet { name: a, value: StructGet { type_id, field: "0", expr: LocalGet(T) } }
/// LocalSet { name: b, value: StructGet { type_id, field: "1", expr: LocalGet(T) } }
/// ...
/// ```
///
/// Replaces with:
///
/// ```text
/// MultiValueLocalBind { instr, locals: [Some("a"), Some("b"), ...] }
/// ```
///
/// This eliminates a GC struct allocation per multi-value result.
fn elide_multi_value_structs(instrs: &mut Vec<WirInstr>, types: &[WirTypeDef]) {
    let mut result = Vec::with_capacity(instrs.len());
    let mut i = 0;

    while i < instrs.len() {
        // Step 1: Detect optional DeclareLocal + LocalSet(MultiValueStructNew)
        let (has_declare, set_idx) = match &instrs[i] {
            WirInstr::DeclareLocal { name: dn, .. } if i + 1 < instrs.len() => {
                if is_mv_local_set(&instrs[i + 1], dn) {
                    (true, i + 1)
                } else {
                    result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                    i += 1;
                    continue;
                }
            }
            _ => (false, i),
        };

        // Step 2: Verify LocalSet(MultiValueStructNew) and extract info
        let Some((temp_name, type_id_index)) = extract_mv_info(&instrs[set_idx]) else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };

        // Step 3: Look up struct field count from the type definition
        let Some(WirTypeDef::Struct(s)) = types.get(type_id_index) else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };
        let field_count = s.fields.len();

        // Step 4: Scan consecutive StructGet instructions reading from the temp
        let mut locals: Vec<Option<String>> = vec![None; field_count];
        let mut j = set_idx + 1;

        while j < instrs.len() {
            if let Some((idx, target)) =
                match_field_get(&instrs[j], &temp_name, field_count, &locals)
            {
                locals[idx] = Some(target);
                j += 1;
            } else {
                break;
            }
        }

        // Step 5: Require at least one field consumed
        if locals.iter().all(Option::is_none) {
            if has_declare {
                result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                result.push(std::mem::replace(&mut instrs[set_idx], WirInstr::Nop));
                i = set_idx + 1;
            } else {
                result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                i += 1;
            }
            continue;
        }

        // Step 6: Extract the inner instruction and emit MultiValueLocalBind
        let inner = take_mv_inner(&mut instrs[set_idx]);

        result.push(WirInstr::MultiValueLocalBind {
            instr: inner,
            locals,
        });
        i = j;
    }

    *instrs = result;
}

/// Check if an instruction is `LocalSet { name, value: MultiValueStructNew { .. } }`.
fn is_mv_local_set(instr: &WirInstr, expected_name: &str) -> bool {
    matches!(
        instr,
        WirInstr::LocalSet { name, value }
            if name == expected_name
               && matches!(value.as_ref(), WirInstr::MultiValueStructNew { .. })
    )
}

/// Extract `(temp_name, type_id_index)` from a `LocalSet(MultiValueStructNew)`.
fn extract_mv_info(instr: &WirInstr) -> Option<(String, usize)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    let WirInstr::MultiValueStructNew { type_id, .. } = value.as_ref() else {
        return None;
    };
    Some((name.clone(), type_id.index() as usize))
}

/// Take ownership of the inner instruction from a `LocalSet(MultiValueStructNew)`.
///
/// Replaces the instruction in-place with `Nop`.
/// Returns `Box<WirInstr>` to pass directly into `MultiValueLocalBind::instr`.
#[allow(clippy::unnecessary_box_returns)]
fn take_mv_inner(instr: &mut WirInstr) -> Box<WirInstr> {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let WirInstr::MultiValueStructNew { instr: inner, .. } = *value else {
        unreachable!()
    };
    inner
}

/// Try to match a `LocalSet { name, value: StructGet { field: "N", expr: LocalGet(temp) } }`.
///
/// Returns `Some((field_index, target_local_name))` on success.
fn match_field_get(
    instr: &WirInstr,
    temp_name: &str,
    field_count: usize,
    already_bound: &[Option<String>],
) -> Option<(usize, String)> {
    let WirInstr::LocalSet {
        name: target,
        value,
    } = instr
    else {
        return None;
    };
    let WirInstr::StructGet {
        field_name, expr, ..
    } = value.as_ref()
    else {
        return None;
    };
    let WirInstr::LocalGet { name: gn } = expr.as_ref() else {
        return None;
    };
    if gn != temp_name {
        return None;
    }
    let idx: usize = field_name.parse().ok()?;
    if idx >= field_count || already_bound[idx].is_some() {
        return None;
    }
    Some((idx, target.clone()))
}
