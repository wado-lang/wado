//! Peephole optimization and cleanup passes for WIR.
//!
//! - **`optimize_instrs`**: constant folding, copy elision, multi-value struct elision.
//! - **`cleanup_wir`**: removes dead locals, nops, and redundant ref.as_non_null.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule, WirTypeDef};

/// Recursively optimize a list of instructions.
///
/// First descends into nested instruction bodies (Block, Loop, If, Seq),
/// then applies flat-level optimizations on the current list.
pub(super) fn optimize_instrs(instrs: &mut Vec<WirInstr>, types: &[WirTypeDef]) {
    for instr in instrs.iter_mut() {
        optimize_nested(instr, types);
    }
    fold_constant_comparisons(instrs);
    elide_redundant_value_copies(instrs);
    elide_multi_value_structs(instrs, types);
}

/// Recurse into nested instruction bodies and eliminate dead branches.
fn optimize_nested(instr: &mut WirInstr, types: &[WirTypeDef]) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            optimize_instrs(body, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            result,
        } => {
            optimize_instrs(then_body, types);
            if let Some(eb) = else_body {
                optimize_instrs(eb, types);
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
            optimize_instrs(body, types);
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


pub(super) fn cleanup_wir(module: &mut WirModule) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
            eliminate_dead_locals(body);
            cleanup_instrs(body);
        }
    }
}

/// Remove `DeclareLocal` instructions for locals that are never referenced
/// by any `LocalGet`, `LocalSet`, `LocalTee`, or `MultiValueLocalBind`.
fn eliminate_dead_locals(body: &mut [WirInstr]) {
    let mut used: IndexSet<String> = IndexSet::default();
    for instr in body.iter() {
        collect_local_uses(instr, &mut used);
    }
    for instr in body.iter_mut() {
        nop_unused_declare_locals(instr, &used);
    }
}

fn collect_local_uses(instr: &WirInstr, used: &mut IndexSet<String>) {
    match instr {
        WirInstr::LocalGet { name } => {
            used.insert(name.clone());
        }
        WirInstr::LocalSet { name, value } => {
            used.insert(name.clone());
            collect_local_uses(value, used);
        }
        WirInstr::LocalTee { name, value } => {
            used.insert(name.clone());
            collect_local_uses(value, used);
        }
        WirInstr::MultiValueLocalBind { instr, locals } => {
            collect_local_uses(instr, used);
            for local in locals.iter().flatten() {
                used.insert(local.clone());
            }
        }
        WirInstr::DeclareLocal { .. } | WirInstr::Nop => {}
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body {
                collect_local_uses(i, used);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            collect_local_uses(condition, used);
            for i in then_body {
                collect_local_uses(i, used);
            }
            if let Some(eb) = else_body {
                for i in eb {
                    collect_local_uses(i, used);
                }
            }
        }
        _ => {
            instr.for_each_child(&mut |child| collect_local_uses(child, used));
        }
    }
}

fn nop_unused_declare_locals(instr: &mut WirInstr, used: &IndexSet<String>) {
    match instr {
        WirInstr::DeclareLocal { name, .. } => {
            if !used.contains(name.as_str()) {
                *instr = WirInstr::Nop;
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body {
                nop_unused_declare_locals(i, used);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for i in then_body {
                nop_unused_declare_locals(i, used);
            }
            if let Some(eb) = else_body {
                for i in eb {
                    nop_unused_declare_locals(i, used);
                }
            }
        }
        _ => {}
    }
}

fn cleanup_instrs(instrs: &mut Vec<WirInstr>) {
    for instr in instrs.iter_mut() {
        cleanup_instr(instr);
    }
    // Remove nops.
    instrs.retain(|i| !matches!(i, WirInstr::Nop));
    // Truncate after first unreachable (dead code elimination).
    if let Some(pos) = instrs
        .iter()
        .position(|i| matches!(i, WirInstr::Unreachable))
    {
        instrs.truncate(pos + 1);
    }
}

fn cleanup_instr(instr: &mut WirInstr) {
    // Recurse into nested bodies first (bottom-up).
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            cleanup_instrs(body);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            cleanup_instr(condition);
            cleanup_instrs(then_body);
            if let Some(eb) = else_body {
                cleanup_instrs(eb);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| cleanup_instr(child));
        }
    }
    // Elide redundant RefAsNonNull wrapping a non-null-producing instruction.
    if let WirInstr::RefAsNonNull(inner) = instr
        && inner.is_nonnull_result()
    {
        *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
    }
}

