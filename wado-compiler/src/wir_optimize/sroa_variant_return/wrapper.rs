//! Resolving a call-site value through the `Block` / `Seq` wrappers lowering
//! and inlining leave around it.

use crate::wir::WirInstr;

/// Resolve the value-producing leaf of a call-site RHS through `Block` / `Seq`
/// wrappers, verifying that stripping the wrappers is sound.
///
/// Soundness of stripping requires:
/// - No prefix statement may branch to *any* `Block` frame being stripped
///   — not just the innermost one: a `BrIf { depth: 1 }` in a nested
///   block's prefix targeting the outer block would dangle once both
///   wrappers are removed.
/// - A break-with-value `Br` must target the block it exits (`depth == 0`);
///   a deeper target carries the value elsewhere.
/// - A trailing `Unreachable` after a break-with-value (appended by
///   `translate_stmts_as_value` so the Wasm validator sees no fallthrough
///   value) is dead code — skipped in `Block` bodies only; a `Seq` has no
///   such emitter convention, so its trailing `Unreachable` means "then
///   trap" and blocks unwrapping.
fn resolve_wrapped_result(instr: &WirInstr, stripped_blocks: u32) -> Option<&WirInstr> {
    match instr {
        WirInstr::Seq(body) => {
            let (last, prefix) = body.split_last()?;
            if any_branch_targets_enclosing(prefix, stripped_blocks) {
                return None;
            }
            resolve_wrapped_result(last, stripped_blocks)
        }
        WirInstr::Block { body, .. } => {
            let effective_len = if matches!(body.last(), Some(WirInstr::Unreachable)) {
                body.len() - 1
            } else {
                body.len()
            };
            let value_idx = effective_len.checked_sub(1)?;
            if any_branch_targets_enclosing(&body[..value_idx], stripped_blocks + 1) {
                return None;
            }
            let last = &body[value_idx];
            if let WirInstr::Seq(seq) = last
                && let Some((WirInstr::Br { depth }, rest)) = seq.split_last()
            {
                if *depth != 0 {
                    return None;
                }
                let value_idx = rest.len().checked_sub(1)?;
                if any_branch_targets_enclosing(&rest[..value_idx], stripped_blocks + 1) {
                    return None;
                }
                return resolve_wrapped_result(&rest[value_idx], stripped_blocks + 1);
            }
            resolve_wrapped_result(last, stripped_blocks + 1)
        }
        other => Some(other),
    }
}

/// True when any instruction in `instrs` branches to one of the `frame_count`
/// innermost enclosing block frames (relative depths `0..frame_count` at this
/// position; nested control frames shift the window by one per level).
fn any_branch_targets_enclosing(instrs: &[WirInstr], frame_count: u32) -> bool {
    if frame_count == 0 {
        return false;
    }
    instrs
        .iter()
        .any(|instr| instr_branches_into_range(instr, 0, frame_count))
}

fn instr_branches_into_range(instr: &WirInstr, base: u32, count: u32) -> bool {
    let in_range = |d: u32| d >= base && d - base < count;
    match instr {
        WirInstr::Br { depth } => in_range(*depth),
        WirInstr::BrIf { depth, condition } => {
            in_range(*depth) || instr_branches_into_range(condition, base, count)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets.iter().copied().any(in_range)
                || in_range(*default)
                || instr_branches_into_range(index, base, count)
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            instr_branches_into_range(condition, base, count)
                || then_body
                    .iter()
                    .any(|i| instr_branches_into_range(i, base + 1, count))
                || else_body.as_ref().is_some_and(|eb| {
                    eb.iter()
                        .any(|i| instr_branches_into_range(i, base + 1, count))
                })
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => body
            .iter()
            .any(|i| instr_branches_into_range(i, base + 1, count)),
        WirInstr::Seq(items) => items
            .iter()
            .any(|i| instr_branches_into_range(i, base, count)),
        // Other instructions cannot introduce a label, but their operands can
        // still embed control flow (e.g. a `BranchHint`-wrapped condition or a
        // labeled block in value position), so recurse at the same depth — the
        // label-introducing arms above adjust it where needed.
        other => {
            let mut found = false;
            other.for_each_child(&mut |child| {
                found = found || instr_branches_into_range(child, base, count);
            });
            found
        }
    }
}

/// Unwrap through `Block` / `Seq` wrappers to the inner `Call` instruction, for
/// arg checking.
pub(super) fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match resolve_wrapped_result(instr, 0) {
        Some(call @ WirInstr::Call { .. }) => Some(call),
        _ => None,
    }
}
