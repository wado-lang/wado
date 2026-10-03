//! Branch-hint passes. `infer_branch_hints` derives hints from control flow
//! rather than source markers: an `if` arm always reaching an `unreachable` is
//! cold, a `br_if` whose fall-through always traps is likely taken. Divergence
//! alone never counts — only a trap does — and an explicit `cold_path()` hint
//! always wins. Runs at every `-O`, so hints stay level-independent.
//!
//! `select_br_ifs` collapses an else-less `if cond { br N }` into `br_if N-1`,
//! carrying the condition's hint over. It runs first, so the trap-tail rule
//! sees the selected `br_if`s.

use crate::wir::{WirInstr, WirPackage};

pub fn infer_branch_hints(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            infer_in_body(body);
        }
    }
}

/// How control leaves an instruction, summarized bottom-up once so no question
/// re-walks a subtree.
#[derive(Default)]
struct Flow {
    /// Always reaches an `unreachable` before any transfer out of it.
    traps: bool,
    /// The furthest label outside it a branch inside targets, `u32::MAX` for a
    /// `return`; `None` when control can leave only by falling through.
    escape: Option<u32>,
    /// The flows of a `Seq`'s instructions, which splice into the enclosing line.
    spliced: Vec<Flow>,
}

impl Flow {
    fn escaping(escape: Option<u32>) -> Self {
        Self {
            escape,
            ..Self::default()
        }
    }

    /// A body's flow as seen from outside the label wrapping it.
    fn through_label(&self) -> Option<u32> {
        match self.escape? {
            u32::MAX => Some(u32::MAX),
            depth => depth.checked_sub(1),
        }
    }
}

/// Whether a straight line traps before anything leaves it, and how far out
/// it can branch.
fn line_flow(flows: Vec<Flow>) -> Flow {
    let traps = flows
        .iter()
        .find(|flow| flow.traps || flow.escape.is_some())
        .is_some_and(|flow| flow.traps);
    Flow {
        traps,
        escape: flows.iter().filter_map(|flow| flow.escape).max(),
        spliced: flows,
    }
}

fn infer_in_body(body: &mut [WirInstr]) -> Flow {
    let flows: Vec<Flow> = body.iter_mut().map(infer_in_instr).collect();
    hint_brif_trap_tail(body, &flows, false);
    line_flow(flows)
}

fn infer_in_instr(instr: &mut WirInstr) -> Flow {
    match instr {
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            let condition_flow = infer_in_instr(condition);
            let then_flow = infer_in_body(then_body);
            let else_flow = else_body.as_deref_mut().map(infer_in_body);
            let else_traps = else_flow.as_ref().is_some_and(|flow| flow.traps);
            // Only a single trapping side yields an unambiguous hint.
            match (then_flow.traps, else_traps) {
                (true, false) => WirInstr::hint_condition(condition, false),
                (false, true) => WirInstr::hint_condition(condition, true),
                _ => {}
            }
            Flow {
                traps: condition_flow.escape.is_none() && then_flow.traps && else_traps,
                escape: [
                    condition_flow.escape,
                    then_flow.through_label(),
                    else_flow.and_then(|flow| flow.through_label()),
                ]
                .into_iter()
                .flatten()
                .max(),
                spliced: Vec::new(),
            }
        }
        // `br 0` targets the block's own end and counts as an escape: conservative
        // for a loop, whose `br 0` re-enters.
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            let flow = infer_in_body(body);
            Flow {
                traps: flow.traps,
                escape: flow.through_label(),
                spliced: Vec::new(),
            }
        }
        WirInstr::Seq(body) => infer_in_body(body),
        WirInstr::Unreachable => Flow {
            traps: true,
            ..Flow::default()
        },
        WirInstr::Return { value } => {
            if let Some(value) = value {
                infer_in_instr(value);
            }
            Flow::escaping(Some(u32::MAX))
        }
        WirInstr::Br { depth } => Flow::escaping(Some(*depth)),
        WirInstr::BrIf { depth, condition } => {
            Flow::escaping(infer_in_instr(condition).escape.max(Some(*depth)))
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            let furthest = targets
                .iter()
                .chain(std::iter::once(&*default))
                .max()
                .copied();
            Flow::escaping(infer_in_instr(index).escape.max(furthest))
        }
        other => {
            let mut escape = None;
            other.for_each_boxed_child_mut(&mut |c| escape = escape.max(infer_in_instr(c).escape));
            Flow::escaping(escape)
        }
    }
}

/// Hints a `br_if` likely where its fall-through unavoidably traps. `reaches_trap`
/// answers that for the path after `instrs`, and the result for the path before.
fn hint_brif_trap_tail(instrs: &mut [WirInstr], flows: &[Flow], mut reaches_trap: bool) -> bool {
    for (instr, flow) in instrs.iter_mut().zip(flows).rev() {
        if reaches_trap && let WirInstr::BrIf { condition, .. } = instr {
            WirInstr::hint_condition(condition, true);
        }
        // Update `reaches_trap` for the position before `instr`.
        match instr {
            WirInstr::Seq(body) => {
                reaches_trap = hint_brif_trap_tail(body, &flow.spliced, reaches_trap);
            }
            _ if flow.traps => reaches_trap = true,
            // Control may leave here, so the path before need not reach the trap.
            _ if flow.escape.is_some() => reaches_trap = false,
            _ => {}
        }
    }
    reaches_trap
}

pub fn select_br_ifs(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                select_in_instr(instr);
            }
        }
    }
}

fn select_in_instr(instr: &mut WirInstr) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body.iter_mut() {
                select_in_instr(i);
            }
        }
        WirInstr::If {
            condition,
            result,
            then_body,
            else_body,
        } => {
            select_in_instr(condition);
            for i in then_body.iter_mut() {
                select_in_instr(i);
            }
            if let Some(eb) = else_body {
                for i in eb.iter_mut() {
                    select_in_instr(i);
                }
            }
            // `if cond { br N }` (no else, no result) ⇔ `br_if N-1`. The `If`
            // introduces a label, so the inner depth shifts down by one;
            // `br 0` (targeting the `if` itself) is a no-op jump and is left
            // alone. `Nop` / `ColdPath` markers in the arm emit nothing and
            // are dropped.
            if result.is_none()
                && else_is_empty(else_body)
                && let Some(depth) = br_only_arm(then_body)
                && depth >= 1
            {
                let cond = std::mem::replace(condition, Box::new(WirInstr::Nop));
                *instr = WirInstr::BrIf {
                    depth: depth - 1,
                    condition: cond,
                };
            }
        }
        other => other.for_each_boxed_child_mut(&mut |c| select_in_instr(c)),
    }
}

/// `Some(depth)` when the arm consists of exactly one `br`, plus any number
/// of no-op markers around it.
fn br_only_arm(body: &[WirInstr]) -> Option<u32> {
    let mut depth = None;
    for instr in body {
        match instr {
            WirInstr::Nop | WirInstr::ColdPath => {}
            WirInstr::Br { depth: d } if depth.is_none() => depth = Some(*d),
            _ => return None,
        }
    }
    depth
}

/// True when an `if` has no meaningful `else` — either no else block, or one
/// whose only instructions are no-op markers (`Nop`, or a `ColdPath` marker
/// that emits nothing). Mirrors `br_only_arm`'s marker tolerance so `br_if`
/// selection is not blocked asymmetrically when a cold-path marker lands in
/// the else arm (the `if let` / `while let` desugaring shape).
fn else_is_empty(else_body: &Option<Vec<WirInstr>>) -> bool {
    match else_body {
        None => true,
        Some(body) => body
            .iter()
            .all(|i| matches!(i, WirInstr::Nop | WirInstr::ColdPath)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::WirType;
    use std::assert_matches;

    fn local_get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn if_instr(
        condition: WirInstr,
        then_body: Vec<WirInstr>,
        else_body: Option<Vec<WirInstr>>,
    ) -> WirInstr {
        WirInstr::If {
            condition: Box::new(condition),
            result: None,
            then_body,
            else_body,
        }
    }

    fn hint_of(instr: &WirInstr) -> Option<bool> {
        let WirInstr::If { condition, .. } = instr else {
            panic!("expected If, got {instr:?}");
        };
        match condition.as_ref() {
            WirInstr::BranchHint { likely, .. } => Some(*likely),
            _ => None,
        }
    }

    #[test]
    fn trapping_then_arm_is_hinted_unlikely() {
        let mut body = vec![if_instr(local_get("c"), vec![WirInstr::Unreachable], None)];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), Some(false));
    }

    #[test]
    fn trapping_else_arm_is_hinted_likely() {
        let mut body = vec![if_instr(
            local_get("c"),
            vec![WirInstr::Return { value: None }],
            Some(vec![WirInstr::Unreachable]),
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), Some(true));
    }

    #[test]
    fn trap_behind_seq_wrapper_is_detected() {
        // The never-typed call shape: `Seq([call, Unreachable])`.
        let mut body = vec![if_instr(
            local_get("c"),
            vec![
                WirInstr::ColdPath,
                WirInstr::Seq(vec![WirInstr::Nop, WirInstr::Unreachable]),
            ],
            None,
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), Some(false));
    }

    #[test]
    fn diverging_arms_are_not_traps() {
        // break / return diverge but must not be hinted cold.
        let mut body = vec![
            if_instr(local_get("a"), vec![WirInstr::Br { depth: 1 }], None),
            if_instr(local_get("b"), vec![WirInstr::Return { value: None }], None),
        ];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), None);
        assert_eq!(hint_of(&body[1]), None);
    }

    #[test]
    fn escaping_br_before_trap_blocks_the_hint() {
        // `br 1` from inside a block escapes the arm past the trap: the arm
        // does not always trap, so no hint.
        let mut body = vec![if_instr(
            local_get("c"),
            vec![
                WirInstr::Block {
                    label: None,
                    result: None,
                    body: vec![WirInstr::BrIf {
                        depth: 1,
                        condition: Box::new(local_get("d")),
                    }],
                },
                WirInstr::Unreachable,
            ],
            None,
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), None);
    }

    #[test]
    fn non_escaping_trap_inside_block_is_hinted() {
        // The trap sits inside a labeled block that nothing branches out of;
        // control still unavoidably reaches it, so the arm is cold.
        let mut body = vec![if_instr(
            local_get("c"),
            vec![WirInstr::Block {
                label: None,
                result: None,
                body: vec![WirInstr::Unreachable],
            }],
            None,
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), Some(false));
    }

    #[test]
    fn inner_br_resolving_inside_arm_still_traps() {
        // `br 0` targets the block itself; control falls through to the trap.
        let mut body = vec![if_instr(
            local_get("c"),
            vec![
                WirInstr::Block {
                    label: None,
                    result: None,
                    body: vec![WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(local_get("d")),
                    }],
                },
                WirInstr::Unreachable,
            ],
            None,
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), Some(false));
    }

    #[test]
    fn existing_hint_wins_over_inference() {
        let mut body = vec![if_instr(
            WirInstr::BranchHint {
                likely: true,
                expr: Box::new(local_get("c")),
            },
            vec![WirInstr::Unreachable],
            None,
        )];
        infer_in_body(&mut body);
        // Inference says unlikely, but the explicit hint must stay.
        assert_eq!(hint_of(&body[0]), Some(true));
    }

    #[test]
    fn both_arms_trapping_yields_no_hint() {
        let mut body = vec![if_instr(
            local_get("c"),
            vec![WirInstr::Unreachable],
            Some(vec![WirInstr::Unreachable]),
        )];
        infer_in_body(&mut body);
        assert_eq!(hint_of(&body[0]), None);
    }

    #[test]
    fn brif_with_trapping_fall_through_is_hinted_likely() {
        let mut body = vec![
            WirInstr::BrIf {
                depth: 1,
                condition: Box::new(local_get("c")),
            },
            WirInstr::Unreachable,
        ];
        infer_in_body(&mut body);
        let WirInstr::BrIf { condition, .. } = &body[0] else {
            panic!("expected BrIf");
        };
        assert_matches!(
            condition.as_ref(),
            WirInstr::BranchHint { likely: true, .. }
        );
    }

    #[test]
    fn brif_with_non_trapping_fall_through_is_not_hinted() {
        let mut body = vec![
            WirInstr::BrIf {
                depth: 1,
                condition: Box::new(local_get("c")),
            },
            WirInstr::Br { depth: 0 },
        ];
        infer_in_body(&mut body);
        let WirInstr::BrIf { condition, .. } = &body[0] else {
            panic!("expected BrIf");
        };
        assert!(!matches!(condition.as_ref(), WirInstr::BranchHint { .. }));
    }

    #[test]
    fn select_collapses_break_guard_to_br_if() {
        let mut instr = if_instr(local_get("c"), vec![WirInstr::Br { depth: 2 }], None);
        select_in_instr(&mut instr);
        let WirInstr::BrIf { depth, condition } = &instr else {
            panic!("expected BrIf, got {instr:?}");
        };
        assert_eq!(*depth, 1);
        assert_matches!(condition.as_ref(), WirInstr::LocalGet { .. });
    }

    #[test]
    fn select_keeps_branch_hint_on_condition() {
        let mut instr = if_instr(
            WirInstr::BranchHint {
                likely: false,
                expr: Box::new(local_get("c")),
            },
            vec![WirInstr::ColdPath, WirInstr::Br { depth: 1 }],
            None,
        );
        select_in_instr(&mut instr);
        let WirInstr::BrIf { depth, condition } = &instr else {
            panic!("expected BrIf, got {instr:?}");
        };
        assert_eq!(*depth, 0);
        assert_matches!(
            condition.as_ref(),
            WirInstr::BranchHint { likely: false, .. }
        );
    }

    #[test]
    fn select_skips_self_targeting_br() {
        // `br 0` targets the `if` itself; rewriting would change the target.
        let mut instr = if_instr(local_get("c"), vec![WirInstr::Br { depth: 0 }], None);
        select_in_instr(&mut instr);
        assert_matches!(instr, WirInstr::If { .. });
    }

    #[test]
    fn select_skips_if_with_meaningful_else() {
        let mut instr = if_instr(
            local_get("c"),
            vec![WirInstr::Br { depth: 1 }],
            Some(vec![WirInstr::Return { value: None }]),
        );
        select_in_instr(&mut instr);
        assert_matches!(instr, WirInstr::If { .. });
    }

    #[test]
    fn select_allows_nop_only_else() {
        // The `if let` desugaring leaves a nop-only else; it is still a pure
        // break guard.
        let mut instr = if_instr(
            local_get("c"),
            vec![WirInstr::Br { depth: 1 }],
            Some(vec![WirInstr::Nop]),
        );
        select_in_instr(&mut instr);
        assert_matches!(instr, WirInstr::BrIf { depth: 0, .. });
    }

    #[test]
    fn select_allows_cold_path_marker_in_else() {
        // A `ColdPath` marker emits nothing, so an else made only of markers is
        // still empty — `br_if` selection must not be blocked asymmetrically
        // (`br_only_arm` already tolerates the same markers in the then arm).
        let mut instr = if_instr(
            local_get("c"),
            vec![WirInstr::Br { depth: 1 }],
            Some(vec![WirInstr::ColdPath, WirInstr::Nop]),
        );
        select_in_instr(&mut instr);
        assert_matches!(instr, WirInstr::BrIf { depth: 0, .. });
    }
}
