//! Loop-guarded bounds check elimination for WIR.
//!
//! When a loop condition `i < arr.len()` dominates the bounds check `i >= arr.used`
//! inside the loop body, the check is provably redundant and can be eliminated.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule};

fn contains_unreachable(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::Unreachable => true,
        WirInstr::Seq(items) => items.iter().any(contains_unreachable),
        _ => false,
    }
}

/// Eliminate bounds checks inside loops when the loop guard already guarantees
/// the index is in-bounds.
///
/// Matches the pattern produced by array indexing inside a for-loop:
///
/// ```text
/// loop {
///     if (i < bound) == 0 { break }     ← loop guard: i < bound
///     ...
///     __copy = i;
///     if __copy >= bound { panic }       ← redundant bounds check
///     array_get(repr, __copy);
///     ...
///     i = i + 1;
///     continue;
/// }
/// ```
///
/// When `i < bound` is known to hold at the loop guard, any `i >= bound`
/// (or copy-of-i >= bound) check inside the same iteration is provably false
/// and can be eliminated.
pub(super) fn eliminate_loop_guarded_bounds_checks(module: &mut WirModule) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        elim_loop_bounds_in_body(body);
    }
}

fn elim_loop_bounds_in_body(body: &mut [WirInstr]) {
    for instr in body.iter_mut() {
        elim_loop_bounds_in_instr(instr);
    }
}

fn elim_loop_bounds_in_instr(instr: &mut WirInstr) {
    match instr {
        WirInstr::Loop { body, .. } => {
            // Try to extract the loop guard pattern and eliminate bounds checks.
            elim_bounds_in_guarded_loop(body);
            // Recurse into remaining nested structures.
            elim_loop_bounds_in_body(body);
        }
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            elim_loop_bounds_in_body(body);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            elim_loop_bounds_in_body(then_body);
            if let Some(eb) = else_body {
                elim_loop_bounds_in_body(eb);
            }
        }
        _ => {}
    }
}

/// Represents a range constraint: `var < bound_var` (signed).
struct LoopRangeConstraint {
    /// The loop induction variable (e.g., "i")
    var: String,
    /// The upper bound variable (e.g., "_`licm_used_31`")
    bound: String,
}

/// Try to extract a loop guard from the first instruction of a loop body.
///
/// Matches:
/// - `if (var < bound) == 0 { break }` → `I32Eq(I32LtS(LocalGet(var), LocalGet(bound)), I32Const(0))`
///   which the WIR represents as `If { condition: I32Eqz(I32LtS(...)), then_body: [Br/break] }`
///   or `If { condition: I32Eq(I32LtS(...), I32Const(0)), then_body: [Br/break] }`
fn extract_loop_guard(body: &[WirInstr]) -> Option<LoopRangeConstraint> {
    let first = body.first()?;
    let WirInstr::If {
        condition,
        then_body,
        ..
    } = first
    else {
        return None;
    };

    // The then_body must contain a break (Br to exit the outer block)
    let has_break = then_body.iter().any(|i| matches!(i, WirInstr::Br { .. }));
    if !has_break {
        return None;
    }

    // Extract the comparison from the negated condition.
    // Pattern: `if (var < bound) == 0 { break }` → condition is `I32Eq(I32LtS(var, bound), I32Const(0))`
    let (var, bound) = extract_negated_lt_comparison(condition)?;
    Some(LoopRangeConstraint { var, bound })
}

/// Extract `(var, bound)` from a negated less-than comparison.
///
/// Matches patterns:
/// - `I32Eq(I32LtS(LocalGet(var), LocalGet(bound)), I32Const(0))`
/// - `I32Eqz(I32LtS(LocalGet(var), LocalGet(bound)))`
fn extract_negated_lt_comparison(condition: &WirInstr) -> Option<(String, String)> {
    // Pattern: I32Eq(I32LtS(var, bound), I32Const(0))
    if let WirInstr::I32Eq(lhs, rhs) = condition {
        if let WirInstr::I32Const(0) = rhs.as_ref() {
            return extract_lt_operands(lhs);
        }
        if let WirInstr::I32Const(0) = lhs.as_ref() {
            return extract_lt_operands(rhs);
        }
    }
    // Pattern: I32Eqz(I32LtS(var, bound))
    if let WirInstr::I32Eqz(inner) = condition {
        return extract_lt_operands(inner);
    }
    None
}

/// Extract `(var, bound)` from `I32LtS(LocalGet(var), LocalGet(bound))`.
fn extract_lt_operands(instr: &WirInstr) -> Option<(String, String)> {
    if let WirInstr::I32LtS(lhs, rhs) = instr
        && let WirInstr::LocalGet { name: var } = lhs.as_ref()
        && let WirInstr::LocalGet { name: bound } = rhs.as_ref()
    {
        return Some((var.clone(), bound.clone()));
    }
    None
}

/// Given a loop body with a known guard constraint (`var < bound`), eliminate
/// redundant bounds checks of the form `if (copy >= bound) { panic }` where
/// `copy` is a copy of `var` (assigned via `LocalSet { copy, LocalGet { var } }`).
fn elim_bounds_in_guarded_loop(body: &mut [WirInstr]) {
    let Some(constraint) = extract_loop_guard(body) else {
        return;
    };

    // Collect copies: locals that are set to `constraint.var` within the loop body.
    // These are equivalent to `var` for the purpose of bounds checking.
    let mut equivalent_vars: IndexSet<String> = IndexSet::default();
    equivalent_vars.insert(constraint.var.clone());
    collect_copies_in_body(body, &constraint.var, &mut equivalent_vars);

    // Now find and eliminate bounds check patterns.
    eliminate_bounds_checks_in_body(body, &equivalent_vars, &constraint.bound);
}

/// Collect locals that are copies of `source_var` (via `LocalSet { name, LocalGet { source_var } }`).
fn collect_copies_in_body(body: &[WirInstr], source_var: &str, copies: &mut IndexSet<String>) {
    for instr in body {
        collect_copies_in_instr(instr, source_var, copies);
    }
}

fn collect_copies_in_instr(instr: &WirInstr, source_var: &str, copies: &mut IndexSet<String>) {
    if let WirInstr::LocalSet { name, value } = instr
        && let WirInstr::LocalGet { name: source } = value.as_ref()
        && source == source_var
    {
        copies.insert(name.clone());
    }
    // Recurse into all children (blocks, if bodies, nested expressions)
    instr.for_each_child(&mut |child| {
        collect_copies_in_instr(child, source_var, copies);
    });
}

/// Replace `if (copy >= bound) { panic; unreachable }` with `Nop` when `copy` is
/// known to be less than `bound` from the loop guard.
fn eliminate_bounds_checks_in_body(
    body: &mut [WirInstr],
    guarded_vars: &IndexSet<String>,
    bound: &str,
) {
    for instr in body.iter_mut() {
        eliminate_bounds_checks_in_instr(instr, guarded_vars, bound);
    }
}

fn eliminate_bounds_checks_in_instr(
    instr: &mut WirInstr,
    guarded_vars: &IndexSet<String>,
    bound: &str,
) {
    // Match the bounds check pattern:
    // If { condition: I32GeS(LocalGet(copy), LocalGet(bound)),
    //      then_body: [panic, unreachable], else_body: None }
    if is_bounds_check_for(instr, guarded_vars, bound) {
        *instr = WirInstr::Nop;
        return;
    }

    // Recurse into nested structures (but NOT into inner loops — they have
    // their own guards and the outer guard may not apply)
    match instr {
        WirInstr::Loop { .. } => {}
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            eliminate_bounds_checks_in_body(body, guarded_vars, bound);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            eliminate_bounds_checks_in_body(then_body, guarded_vars, bound);
            if let Some(eb) = else_body {
                eliminate_bounds_checks_in_body(eb, guarded_vars, bound);
            }
        }
        _ => {
            // Recurse into all children (e.g., Block inside LocalSet value,
            // I32Add operands, etc.)
            instr.for_each_boxed_child_mut(&mut |child| {
                eliminate_bounds_checks_in_instr(child, guarded_vars, bound);
            });
        }
    }
}

/// Check if an instruction is a bounds check `if (var >= bound) { panic; unreachable }`
/// where `var` is one of the guarded variables and `bound` is the loop bound.
fn is_bounds_check_for(instr: &WirInstr, guarded_vars: &IndexSet<String>, bound: &str) -> bool {
    let WirInstr::If {
        condition,
        then_body,
        else_body,
        ..
    } = instr
    else {
        return false;
    };

    // Must have no else branch
    if else_body.is_some() {
        return false;
    }

    // then_body must contain unreachable (typical panic pattern),
    // possibly nested inside a Seq
    if !then_body.iter().any(contains_unreachable) {
        return false;
    }

    // Condition must be `I32GeS(LocalGet(var), LocalGet(bound))`
    // where var is in guarded_vars and bound matches
    if let WirInstr::I32GeS(lhs, rhs) = condition.as_ref()
        && let WirInstr::LocalGet { name: var } = lhs.as_ref()
        && let WirInstr::LocalGet { name: b } = rhs.as_ref()
    {
        return guarded_vars.contains(var) && b == bound;
    }

    false
}
