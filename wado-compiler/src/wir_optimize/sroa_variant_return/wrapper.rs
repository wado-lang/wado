//! Resolving a call-site value through the `Block` / `Seq` wrappers lowering
//! and inlining leave around it.
//!
//! One shape definition shared by validation and rewrite, so accept and rewrite
//! cannot disagree on where the call and its prefix statements live.

use crate::hashmap::IndexSet;
use crate::wir::WirInstr;

/// One step of the path from a wrapped call-site value to its result leaf.
/// `value_idx` is the index of the value-producing element in the wrapper's
/// instruction list; earlier elements are prefix statements the rewriter
/// hoists out of the wrapper.
#[derive(Clone, Copy)]
pub(super) enum ResultStep {
    /// `Seq(body)`: value is `body[value_idx]` (the last element).
    Seq { value_idx: usize },
    /// `Block { body }`: value is `body[value_idx]` (the effective last
    /// element after trimming a trailing `Unreachable`).
    Block { value_idx: usize },
    /// A `Seq([.., value, Br(0)])` break-with-value at a `Block`'s result
    /// position: value is `seq[value_idx]` (`seq.len() - 2`).
    BreakValue { value_idx: usize },
}

/// Resolve the value-producing leaf of a call-site RHS through `Block` /
/// `Seq` wrappers, verifying that stripping the wrappers is sound. The one
/// shape definition shared by validation (`unwrap_to_candidate_call`,
/// `unwrap_to_inner_call`, `validate_wrapper_prefixes`) and the
/// rewriter (`take_call_from_local_set`), so accept and rewrite cannot
/// diverge.
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
pub(super) fn resolve_wrapped_result(instr: &WirInstr) -> Option<(Vec<ResultStep>, &WirInstr)> {
    let mut steps = Vec::new();
    let leaf = resolve_wrapped_result_inner(instr, 0, &mut steps)?;
    Some((steps, leaf))
}

fn resolve_wrapped_result_inner<'a>(
    instr: &'a WirInstr,
    stripped_blocks: u32,
    steps: &mut Vec<ResultStep>,
) -> Option<&'a WirInstr> {
    match instr {
        WirInstr::Seq(body) => {
            let (last, prefix) = body.split_last()?;
            if any_branch_targets_enclosing(prefix, stripped_blocks) {
                return None;
            }
            steps.push(ResultStep::Seq {
                value_idx: body.len() - 1,
            });
            resolve_wrapped_result_inner(last, stripped_blocks, steps)
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
            steps.push(ResultStep::Block { value_idx });
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
                steps.push(ResultStep::BreakValue { value_idx });
                return resolve_wrapped_result_inner(&rest[value_idx], stripped_blocks + 1, steps);
            }
            resolve_wrapped_result_inner(last, stripped_blocks + 1, steps)
        }
        other => Some(other),
    }
}

/// True when any instruction in `instrs` branches to one of the `frame_count`
/// innermost enclosing block frames (relative depths `0..frame_count` at this
/// position; nested control frames shift the window by one per level).
pub(super) fn any_branch_targets_enclosing(instrs: &[WirInstr], frame_count: u32) -> bool {
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

/// Look through trivial `Block` / `Seq` wrappers to find a `Call` to a
/// candidate function at the result position. Returns the `func_id` index.
///
/// `LocalSet`-from-`Call` site-effects are often wrapped in a `Seq` in WIR —
/// e.g. `LocalSet(name, Seq([prefix..., Call(f)]))` — and inlining wraps
/// results in labeled `Block`s. Without unwrapping,
/// `find_nested_candidate_calls` would mis-classify every such site as a
/// "nested candidate call" and invalidate `f`, even though the pattern is the
/// idiomatic LocalSet-bound call we want to support.
pub(super) fn unwrap_to_candidate_call(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
) -> Option<u32> {
    match resolve_wrapped_result(instr)?.1 {
        WirInstr::Call { func_id, .. } if candidate_ids.contains(&func_id.index()) => {
            Some(func_id.index())
        }
        _ => None,
    }
}

/// Unwrap through `Block` / `Seq` wrappers to the inner `Call` instruction
/// (for arg checking). Shares [`resolve_wrapped_result`] with the candidate
/// check and the rewriter, so it sees exactly the call they see — including
/// through a trailing `Unreachable` after a break-with-value.
pub(super) fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match resolve_wrapped_result(instr) {
        Some((_, call @ WirInstr::Call { .. })) => Some(call),
        _ => None,
    }
}

/// Take the Call instruction out of a `LocalSet`, unwrapping trivial
/// `Block` / `Seq` wrappers. Replaces the instruction with Nop. Returns
/// `(prefix_instrs, call_instr)` where prefix instructions are statements
/// from inside the wrappers that must be emitted before the call (e.g.
/// initialization of locals used as call arguments). Consumes the same
/// [`resolve_wrapped_result`] path validation used, so the two cannot
/// disagree on where the call and its prefixes live.
pub(super) fn take_call_from_local_set(instr: &mut WirInstr) -> (Vec<WirInstr>, Box<WirInstr>) {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let (steps, _) = resolve_wrapped_result(&value)
        .unwrap_or_else(|| unreachable!("SROA call-site take on a shape validation rejected"));
    let mut prefix = Vec::new();
    let mut current = *value;
    for step in steps {
        let (mut list, value_idx) = match (step, current) {
            (
                ResultStep::Seq { value_idx } | ResultStep::BreakValue { value_idx },
                WirInstr::Seq(body),
            )
            | (ResultStep::Block { value_idx }, WirInstr::Block { body, .. }) => (body, value_idx),
            _ => unreachable!("resolve_wrapped_result path mismatch"),
        };
        for item in list.drain(..value_idx) {
            if !matches!(item, WirInstr::Nop) {
                prefix.push(item);
            }
        }
        // The value now sits at index 0; the rest (a break `Br`, a trailing
        // `Unreachable`) is dropped with the wrapper.
        current = list.swap_remove(0);
    }
    match current {
        call @ WirInstr::Call { .. } => (prefix, Box::new(call)),
        _ => unreachable!("expected Call at SROA call-site result leaf"),
    }
}
