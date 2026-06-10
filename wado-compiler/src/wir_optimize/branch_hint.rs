//! Branch-hint passes: trap-based hint inference and `br_if` selection.
//!
//! `infer_branch_hints` derives branch hints from control flow instead of
//! source markers: an `if` arm that always reaches an `unreachable` trap is
//! cold, and a `br_if` whose fall-through always traps is likely taken.
//! Divergence alone (`br` / `return`) never counts as cold — a `break` or an
//! early `return` is ordinary control flow, only a trap is. Explicit hints
//! (synthesized from `builtin::cold_path()` at WIR build) always win over
//! inference. The pass runs at every optimization level so hints stay
//! independent of `-O`, matching `apply_cold_path_hints`.
//!
//! `select_br_ifs` collapses `if cond { br N }` with an empty else into a
//! single `br_if N-1`, carrying any branch hint on the condition over to the
//! `br_if`. It must run after `init_guard`, whose matcher keys on the
//! `If { GlobalGet, [Br] }` shape, and before `infer_branch_hints`, so the
//! trap-tail rule sees the selected `br_if`s.

use crate::wir::{WirInstr, WirPackage};

pub fn infer_branch_hints(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            infer_in_body(body);
        }
    }
}

fn infer_in_body(body: &mut [WirInstr]) {
    for instr in body.iter_mut() {
        infer_in_instr(instr);
    }
    hint_brif_trap_tail(body, false);
}

fn infer_in_instr(instr: &mut WirInstr) {
    match instr {
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            infer_in_instr(condition);
            infer_in_body(then_body);
            if let Some(eb) = else_body {
                infer_in_body(eb);
            }
            let then_traps = arm_always_traps(then_body);
            let else_traps = else_body.as_deref().is_some_and(arm_always_traps);
            // Only a single trapping side yields an unambiguous hint.
            let likely = match (then_traps, else_traps) {
                (true, false) => false,
                (false, true) => true,
                _ => return,
            };
            WirInstr::hint_condition(condition, likely);
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            infer_in_body(body);
        }
        other => other.for_each_boxed_child_mut(&mut |c| infer_in_instr(c)),
    }
}

/// Backward pass for the `br_if` trap-tail rule: a `br_if` whose fall-through
/// unavoidably traps must be taken on every well-defined execution, so it is
/// hinted likely. `reaches_trap` is whether the path immediately *after* this
/// slice unavoidably reaches `unreachable`; the return value is the same
/// predicate for the path *before* the slice.
fn hint_brif_trap_tail(instrs: &mut [WirInstr], mut reaches_trap: bool) -> bool {
    for i in (0..instrs.len()).rev() {
        if reaches_trap
            && let WirInstr::BrIf { condition, .. } = &mut instrs[i]
        {
            WirInstr::hint_condition(condition, true);
        }
        // Update `reaches_trap` for the position before `instrs[i]`.
        match &mut instrs[i] {
            WirInstr::Unreachable => reaches_trap = true,
            // `Seq` splices transparently into the straight line.
            WirInstr::Seq(body) => {
                reaches_trap = hint_brif_trap_tail(body, reaches_trap);
            }
            other => {
                if arm_always_traps(std::slice::from_ref(other)) {
                    reaches_trap = true;
                } else if may_transfer_out(other, 0) || other.always_diverges() {
                    // Control may leave (or definitely leaves) through this
                    // instruction; the path before it no longer unavoidably
                    // reaches the trap behind us.
                    reaches_trap = false;
                }
            }
        }
    }
    reaches_trap
}

/// Whether executing this instruction list always reaches an `unreachable`
/// trap before any control transfer out of it. Conservative: any possible
/// escape (a `return`, or a branch whose depth leaves the list) answers
/// `false`.
fn arm_always_traps(instrs: &[WirInstr]) -> bool {
    for instr in instrs {
        match instr {
            WirInstr::Unreachable => return true,
            // `Seq` splices transparently into the straight line.
            WirInstr::Seq(body) => {
                if arm_always_traps(body) {
                    return true;
                }
                if body.iter().any(|i| may_transfer_out(i, 0)) {
                    return false;
                }
            }
            // An `if` whose both arms trap, traps.
            WirInstr::If {
                condition,
                then_body,
                else_body: Some(eb),
                ..
            } if !may_transfer_out(condition, 0)
                && arm_always_traps(then_body)
                && arm_always_traps(eb) =>
            {
                return true;
            }
            other => {
                if may_transfer_out(other, 0) {
                    return false;
                }
            }
        }
    }
    false
}

/// Whether this instruction's subtree can transfer control out of the context
/// it appears in: a `return`, or a `br`/`br_if`/`br_table` whose depth escapes
/// the labels introduced within the subtree itself. `nesting` is the number of
/// labels between `instr` and that context.
fn may_transfer_out(instr: &WirInstr, nesting: u32) -> bool {
    match instr {
        WirInstr::Return { .. } => true,
        WirInstr::Br { depth } => *depth >= nesting,
        WirInstr::BrIf { depth, condition } => {
            *depth >= nesting || may_transfer_out(condition, nesting)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets.iter().chain(std::iter::once(default)).any(|d| *d >= nesting)
                || may_transfer_out(index, nesting)
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            body.iter().any(|i| may_transfer_out(i, nesting + 1))
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            may_transfer_out(condition, nesting)
                || then_body.iter().any(|i| may_transfer_out(i, nesting + 1))
                || else_body
                    .as_ref()
                    .is_some_and(|eb| eb.iter().any(|i| may_transfer_out(i, nesting + 1)))
        }
        WirInstr::Seq(body) => body.iter().any(|i| may_transfer_out(i, nesting)),
        other => {
            let mut found = false;
            other.for_each_child(&mut |c| {
                found = found || may_transfer_out(c, nesting);
            });
            found
        }
    }
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

/// `Some(depth)` when the arm consists solely of no-op markers and a single
/// trailing `br`.
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
/// whose only instructions are `nop`s (the `if let` / `while let` desugaring
/// shape).
fn else_is_empty(else_body: &Option<Vec<WirInstr>>) -> bool {
    match else_body {
        None => true,
        Some(body) => body.iter().all(|i| matches!(i, WirInstr::Nop)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::WirType;

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
        let mut body = vec![if_instr(
            local_get("c"),
            vec![WirInstr::Unreachable],
            None,
        )];
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
            if_instr(
                local_get("b"),
                vec![WirInstr::Return { value: None }],
                None,
            ),
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
        assert!(matches!(
            condition.as_ref(),
            WirInstr::BranchHint { likely: true, .. }
        ));
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
        assert!(matches!(condition.as_ref(), WirInstr::LocalGet { .. }));
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
        assert!(matches!(
            condition.as_ref(),
            WirInstr::BranchHint { likely: false, .. }
        ));
    }

    #[test]
    fn select_skips_self_targeting_br() {
        // `br 0` targets the `if` itself; rewriting would change the target.
        let mut instr = if_instr(local_get("c"), vec![WirInstr::Br { depth: 0 }], None);
        select_in_instr(&mut instr);
        assert!(matches!(instr, WirInstr::If { .. }));
    }

    #[test]
    fn select_skips_if_with_meaningful_else() {
        let mut instr = if_instr(
            local_get("c"),
            vec![WirInstr::Br { depth: 1 }],
            Some(vec![WirInstr::Return { value: None }]),
        );
        select_in_instr(&mut instr);
        assert!(matches!(instr, WirInstr::If { .. }));
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
        assert!(matches!(instr, WirInstr::BrIf { depth: 0, .. }));
    }
}
