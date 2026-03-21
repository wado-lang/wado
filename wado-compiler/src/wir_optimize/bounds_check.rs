//! Loop-guarded bounds check elimination for WIR.
//!
//! When a loop condition `i < arr.len()` or `i <= limit` dominates the bounds
//! check `i >= arr.used` inside the loop body, the check is provably redundant
//! and can be eliminated.
//!
//! For `<=` guards (`i <= limit`), the effective exclusive upper bound is
//! `limit + 1`. The pass resolves definition chains to verify that the bounds
//! check bound equals `guard_bound + 1` (e.g., `arr.used == limit + 1`).

use crate::hashmap::{IndexMap, IndexSet};
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
/// Matches patterns produced by array indexing inside for-loops:
///
/// ```text
/// loop {
///     if (i < bound) == 0 { break }     ← loop guard: i < bound
///     ...
///     if __copy >= bound { panic }       ← redundant bounds check
///     ...
/// }
/// ```
///
/// Also handles `<=` guards:
///
/// ```text
/// __local = limit + 1;
/// arr = StructNew { ..., used: __local };
/// loop {
///     if (i <= limit) == 0 { break }     ← loop guard: i <= limit
///     ...
///     if __copy >= arr_used { panic }     ← redundant (arr_used == limit + 1)
///     ...
/// }
/// ```
pub(super) fn eliminate_loop_guarded_bounds_checks(module: &mut WirModule) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        // Build a definition map for the entire function to resolve value chains.
        let defs = build_definition_map(body);
        elim_loop_bounds_in_body(body, &defs);
    }
}

fn elim_loop_bounds_in_body(body: &mut [WirInstr], defs: &DefinitionMap) {
    for instr in body.iter_mut() {
        elim_loop_bounds_in_instr(instr, defs);
    }
}

fn elim_loop_bounds_in_instr(instr: &mut WirInstr, defs: &DefinitionMap) {
    match instr {
        WirInstr::Loop { body, .. } => {
            // Try to extract the loop guard pattern and eliminate bounds checks.
            elim_bounds_in_guarded_loop(body, defs);
            // Recurse into remaining nested structures.
            elim_loop_bounds_in_body(body, defs);
        }
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            elim_loop_bounds_in_body(body, defs);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            elim_loop_bounds_in_body(then_body, defs);
            if let Some(eb) = else_body {
                elim_loop_bounds_in_body(eb, defs);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
enum GuardKind {
    /// `i < bound` — bound is the exclusive upper bound directly
    StrictLt,
    /// `i <= bound` — effective exclusive bound is `bound + 1`
    LtOrEq,
}

struct LoopRangeConstraint {
    /// The loop induction variable (e.g., "i")
    var: String,
    /// The upper bound variable from the guard
    bound: String,
    /// Whether this is `<` or `<=`
    kind: GuardKind,
}

/// Try to extract a loop guard from the first instruction of a loop body.
///
/// Matches:
/// - `if (var < bound) == 0 { break }` → `StrictLt`
/// - `if (var <= bound) == 0 { break }` → `LtOrEq`
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

    // Try `<` first, then `<=`
    if let Some((var, bound)) = extract_negated_comparison(condition, ComparisonOp::Lt) {
        return Some(LoopRangeConstraint {
            var,
            bound,
            kind: GuardKind::StrictLt,
        });
    }
    if let Some((var, bound)) = extract_negated_comparison(condition, ComparisonOp::Le) {
        return Some(LoopRangeConstraint {
            var,
            bound,
            kind: GuardKind::LtOrEq,
        });
    }
    None
}

enum ComparisonOp {
    Lt,
    Le,
}

/// Extract `(var, bound)` from a negated comparison.
///
/// Matches patterns:
/// - `I32Eq(I32LtS/I32LeS(LocalGet(var), LocalGet(bound)), I32Const(0))`
/// - `I32Eqz(I32LtS/I32LeS(LocalGet(var), LocalGet(bound)))`
fn extract_negated_comparison(condition: &WirInstr, op: ComparisonOp) -> Option<(String, String)> {
    // Pattern: I32Eq(cmp(var, bound), I32Const(0))
    if let WirInstr::I32Eq(lhs, rhs) = condition {
        if let WirInstr::I32Const(0) = rhs.as_ref() {
            return extract_cmp_operands(lhs, &op);
        }
        if let WirInstr::I32Const(0) = lhs.as_ref() {
            return extract_cmp_operands(rhs, &op);
        }
    }
    // Pattern: I32Eqz(cmp(var, bound))
    if let WirInstr::I32Eqz(inner) = condition {
        return extract_cmp_operands(inner, &op);
    }
    None
}

/// Extract `(var, bound)` from `I32LtS/I32LeS(LocalGet(var), LocalGet(bound))`.
fn extract_cmp_operands(instr: &WirInstr, op: &ComparisonOp) -> Option<(String, String)> {
    let (lhs, rhs) = match (op, instr) {
        (ComparisonOp::Lt, WirInstr::I32LtS(lhs, rhs)) => (lhs, rhs),
        (ComparisonOp::Le, WirInstr::I32LeS(lhs, rhs)) => (lhs, rhs),
        _ => return None,
    };
    if let WirInstr::LocalGet { name: var } = lhs.as_ref()
        && let WirInstr::LocalGet { name: bound } = rhs.as_ref()
    {
        return Some((var.clone(), bound.clone()));
    }
    None
}

/// Given a loop body with a known guard constraint, eliminate redundant bounds
/// checks of the form `if (copy >= bound) { panic }`.
fn elim_bounds_in_guarded_loop(body: &mut [WirInstr], defs: &DefinitionMap) {
    let Some(constraint) = extract_loop_guard(body) else {
        return;
    };

    // Collect copies: locals that are set to `constraint.var` within the loop body.
    let mut equivalent_vars: IndexSet<String> = IndexSet::default();
    equivalent_vars.insert(constraint.var.clone());
    collect_copies_in_body(body, &constraint.var, &mut equivalent_vars);

    // Determine which bounds check bounds to accept.
    match constraint.kind {
        GuardKind::StrictLt => {
            // `i < bound` → bounds check `i >= bound` is redundant
            eliminate_bounds_checks_in_body(body, &equivalent_vars, &constraint.bound, defs);
        }
        GuardKind::LtOrEq => {
            // `i <= limit` → effective exclusive bound is `limit + 1`
            // The bounds check uses a variable that equals `limit + 1`
            eliminate_bounds_checks_le_guard(body, &equivalent_vars, &constraint.bound, defs);
        }
    }
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
/// known to be less than `bound` from the loop guard (strict `<` case).
fn eliminate_bounds_checks_in_body(
    body: &mut [WirInstr],
    guarded_vars: &IndexSet<String>,
    bound: &str,
    defs: &DefinitionMap,
) {
    for instr in body.iter_mut() {
        eliminate_bounds_checks_in_instr(instr, guarded_vars, bound, defs);
    }
}

fn eliminate_bounds_checks_in_instr(
    instr: &mut WirInstr,
    guarded_vars: &IndexSet<String>,
    bound: &str,
    defs: &DefinitionMap,
) {
    if is_bounds_check_for(instr, guarded_vars, bound, defs, false) {
        *instr = WirInstr::Nop;
        return;
    }

    // Recurse into nested structures (but NOT into inner loops — they have
    // their own guards and the outer guard may not apply)
    match instr {
        WirInstr::Loop { .. } => {}
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            eliminate_bounds_checks_in_body(body, guarded_vars, bound, defs);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            eliminate_bounds_checks_in_instr(condition, guarded_vars, bound, defs);
            eliminate_bounds_checks_in_body(then_body, guarded_vars, bound, defs);
            if let Some(eb) = else_body {
                eliminate_bounds_checks_in_body(eb, guarded_vars, bound, defs);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                eliminate_bounds_checks_in_instr(child, guarded_vars, bound, defs);
            });
        }
    }
}

/// Eliminate bounds checks for `<=` guards by resolving whether the bounds
/// check bound equals `guard_bound + 1` via definition chains.
fn eliminate_bounds_checks_le_guard(
    body: &mut [WirInstr],
    guarded_vars: &IndexSet<String>,
    guard_bound: &str,
    defs: &DefinitionMap,
) {
    for instr in body.iter_mut() {
        eliminate_bounds_checks_le_guard_instr(instr, guarded_vars, guard_bound, defs);
    }
}

fn eliminate_bounds_checks_le_guard_instr(
    instr: &mut WirInstr,
    guarded_vars: &IndexSet<String>,
    guard_bound: &str,
    defs: &DefinitionMap,
) {
    if is_bounds_check_for(instr, guarded_vars, guard_bound, defs, true) {
        *instr = WirInstr::Nop;
        return;
    }

    match instr {
        WirInstr::Loop { .. } => {}
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            eliminate_bounds_checks_le_guard(body, guarded_vars, guard_bound, defs);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            eliminate_bounds_checks_le_guard_instr(condition, guarded_vars, guard_bound, defs);
            eliminate_bounds_checks_le_guard(then_body, guarded_vars, guard_bound, defs);
            if let Some(eb) = else_body {
                eliminate_bounds_checks_le_guard(eb, guarded_vars, guard_bound, defs);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                eliminate_bounds_checks_le_guard_instr(child, guarded_vars, guard_bound, defs);
            });
        }
    }
}

/// Check if an instruction is a bounds check `if (var >= bound) { panic; unreachable }`
/// where `var` is one of the guarded variables.
///
/// When `is_le_guard` is false: `bound` must match the guard bound directly.
/// When `is_le_guard` is true: `bound` must resolve to `guard_bound + 1` via definitions.
fn is_bounds_check_for(
    instr: &WirInstr,
    guarded_vars: &IndexSet<String>,
    guard_bound: &str,
    defs: &DefinitionMap,
    is_le_guard: bool,
) -> bool {
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

    // then_body must contain unreachable (typical panic pattern)
    if !then_body.iter().any(contains_unreachable) {
        return false;
    }

    // Condition must be `I32GeS(LocalGet(var), LocalGet(check_bound))`
    if let WirInstr::I32GeS(lhs, rhs) = condition.as_ref()
        && let WirInstr::LocalGet { name: var } = lhs.as_ref()
        && let WirInstr::LocalGet { name: check_bound } = rhs.as_ref()
        && guarded_vars.contains(var)
    {
        if is_le_guard {
            // For `<=` guard: check_bound must resolve to guard_bound + 1
            return resolves_to_plus_one(defs, check_bound, guard_bound);
        } else {
            // For `<` guard: check_bound must match guard_bound directly,
            // or resolve to the same variable through copies
            return check_bound == guard_bound
                || resolve_to_same_local(defs, check_bound, guard_bound);
        }
    }

    false
}

type DefinitionMap = IndexMap<String, WirInstr>;

/// Build a map from local variable names to their defining expressions.
/// Only records the *first* definition (SSA-like for compiler-generated temps).
/// Skips locals that are assigned more than once (not safe to resolve).
fn build_definition_map(body: &[WirInstr]) -> DefinitionMap {
    let mut defs = DefinitionMap::default();
    let mut multi_assigned: IndexSet<String> = IndexSet::default();
    collect_definitions(body, &mut defs, &mut multi_assigned);
    // Remove any locals that are assigned multiple times
    for name in &multi_assigned {
        defs.swap_remove(name);
    }
    defs
}

fn collect_definitions(
    body: &[WirInstr],
    defs: &mut DefinitionMap,
    multi_assigned: &mut IndexSet<String>,
) {
    for instr in body {
        collect_definitions_in_instr(instr, defs, multi_assigned);
    }
}

fn collect_definitions_in_instr(
    instr: &WirInstr,
    defs: &mut DefinitionMap,
    multi_assigned: &mut IndexSet<String>,
) {
    if let WirInstr::LocalSet { name, value } = instr {
        if defs.contains_key(name) {
            multi_assigned.insert(name.clone());
        } else {
            defs.insert(name.clone(), value.as_ref().clone());
        }
    }
    // Recurse into all children
    instr.for_each_child(&mut |child| {
        collect_definitions_in_instr(child, defs, multi_assigned);
    });
}

/// Check if `var_name` resolves to `target + 1` by following definition chains.
///
/// Follows copies (`var = other_var`) and checks for `I32Add(target, 1)` patterns.
fn resolves_to_plus_one(defs: &DefinitionMap, var_name: &str, target: &str) -> bool {
    resolves_to_plus_one_inner(defs, var_name, target, 0)
}

fn resolves_to_plus_one_inner(
    defs: &DefinitionMap,
    var_name: &str,
    target: &str,
    depth: usize,
) -> bool {
    if depth > 10 {
        return false; // prevent infinite chains
    }
    let Some(def) = defs.get(var_name) else {
        return false;
    };
    match def {
        // Copy: follow the chain
        WirInstr::LocalGet { name } => resolves_to_plus_one_inner(defs, name, target, depth + 1),
        // target + 1 pattern
        WirInstr::I32Add(lhs, rhs) => {
            if let WirInstr::LocalGet { name } = lhs.as_ref()
                && let WirInstr::I32Const(1) = rhs.as_ref()
            {
                return resolve_to_same_local_or_eq(defs, name, target, 0);
            }
            if let WirInstr::I32Const(1) = lhs.as_ref()
                && let WirInstr::LocalGet { name } = rhs.as_ref()
            {
                return resolve_to_same_local_or_eq(defs, name, target, 0);
            }
            false
        }
        _ => false,
    }
}

/// Check if two locals resolve to the same variable through copy chains.
fn resolve_to_same_local(defs: &DefinitionMap, a: &str, b: &str) -> bool {
    resolve_to_same_local_or_eq(defs, a, b, 0)
}

fn resolve_to_same_local_or_eq(defs: &DefinitionMap, a: &str, b: &str, depth: usize) -> bool {
    if a == b {
        return true;
    }
    if depth > 10 {
        return false;
    }
    // Try resolving `a` one step
    if let Some(WirInstr::LocalGet { name }) = defs.get(a)
        && resolve_to_same_local_or_eq(defs, name, b, depth + 1)
    {
        return true;
    }
    // Try resolving `b` one step
    if let Some(WirInstr::LocalGet { name }) = defs.get(b)
        && resolve_to_same_local_or_eq(defs, a, name, depth + 1)
    {
        return true;
    }
    false
}
