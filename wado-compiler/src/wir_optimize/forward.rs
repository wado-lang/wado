//! Struct field constant forwarding and loop-guarded bounds check elimination for WIR.
//!
//! - **Field constant forwarding**: propagates known constant field values through StructGet.
//! - **Loop-guarded bounds check elimination**: removes redundant bounds checks inside loops.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirModule, WirTypeDef, WirTypeId};

use super::collect_local_gets_deep;

fn contains_unreachable(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::Unreachable => true,
        WirInstr::Seq(items) => items.iter().any(contains_unreachable),
        _ => false,
    }
}

pub(super) fn forward_struct_field_constants(module: &mut WirModule) {
    let types = &module.types;
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        // Collect locals whose references escape. Field forwarding is unsafe
        // for these locals because their fields can be modified through aliases.
        let aliased = collect_aliased_locals(body);
        let mut changed = true;
        while changed {
            let mut known = FieldKnowledge::new(types, &aliased);
            changed = forward_fields_in_body(body, &mut known);
        }
    }
}

/// Collect locals whose references escape (address taken, embedded in structs,
/// or passed to function calls). These are unsafe for field forwarding.
fn collect_aliased_locals(body: &[WirInstr]) -> IndexSet<String> {
    let mut aliased = IndexSet::default();
    for instr in body {
        collect_aliased_in_instr(instr, &mut aliased);
    }
    aliased
}

fn collect_aliased_in_instr(instr: &WirInstr, aliased: &mut IndexSet<String>) {
    match instr {
        // Function calls: all LocalGet args could have their fields modified
        WirInstr::Call { args, .. } | WirInstr::CallRef { args, .. } => {
            for arg in args {
                collect_local_gets_deep(arg, aliased);
            }
        }
        // RefAsNonNull of a LocalGet: address taken
        WirInstr::RefAsNonNull(inner) => {
            if let WirInstr::LocalGet { name } = inner.as_ref() {
                aliased.insert(name.clone());
            }
        }
        // LocalSet from another local: both are aliases of the same GC object.
        // Modifications through either one affect the other.
        WirInstr::LocalSet { name, value } => {
            if let WirInstr::LocalGet { name: source } = value.as_ref() {
                aliased.insert(name.clone());
                aliased.insert(source.clone());
            }
        }
        _ => {}
    }
    // Recurse into children
    instr.for_each_child(&mut |child| {
        collect_aliased_in_instr(child, aliased);
    });
}



/// Known constant field values for locals.
/// Maps `(local_name, field_name)` → constant `WirInstr`.
struct FieldKnowledge<'a> {
    /// Known constant field values: `(local_name, field_name)` → constant value
    fields: IndexMap<(String, String), WirInstr>,
    /// Type definitions for resolving field names by index
    types: &'a [WirTypeDef],
    /// Locals that are aliased and unsafe for field forwarding
    aliased: &'a IndexSet<String>,
}

impl<'a> FieldKnowledge<'a> {
    fn new(types: &'a [WirTypeDef], aliased: &'a IndexSet<String>) -> Self {
        Self {
            fields: IndexMap::default(),
            types,
            aliased,
        }
    }

    /// Record all constant fields from a `StructNew` assigned to `local_name`.
    /// Skips aliased locals (their fields may be modified through references).
    fn record_struct_new(&mut self, local_name: &str, type_id: WirTypeId, fields: &[WirInstr]) {
        if self.aliased.contains(local_name) {
            return;
        }
        let Some(WirTypeDef::Struct(st)) = self.types.get(type_id.index() as usize) else {
            return;
        };
        for (i, field_def) in st.fields.iter().enumerate() {
            if let Some(field_instr) = fields.get(i)
                && is_wir_constant(field_instr)
            {
                self.fields.insert(
                    (local_name.to_string(), field_def.name.clone()),
                    field_instr.clone(),
                );
            }
        }
    }

    /// Look up a known constant for `local_name.field_name`.
    fn get(&self, local_name: &str, field_name: &str) -> Option<&WirInstr> {
        self.fields
            .get(&(local_name.to_string(), field_name.to_string()))
    }

    /// Invalidate all known fields for a local (on reassignment or mutation).
    fn invalidate_local(&mut self, local_name: &str) {
        self.fields.retain(|(name, _), _| name != local_name);
    }

    /// Invalidate a specific field for a local (on `StructSet`).
    fn invalidate_field(&mut self, local_name: &str, field_name: &str) {
        self.fields
            .swap_remove(&(local_name.to_string(), field_name.to_string()));
    }
}

/// Check if a WIR instruction is a constant value.
fn is_wir_constant(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::I32Const(_)
            | WirInstr::I64Const(_)
            | WirInstr::F32Const(_)
            | WirInstr::F64Const(_)
    )
}

/// Process a body (list of instructions), forwarding known constants.
/// Returns true if any changes were made.
fn forward_fields_in_body(body: &mut [WirInstr], known: &mut FieldKnowledge<'_>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < body.len() {
        changed |= forward_fields_in_instr(&mut body[i], known);

        // After processing, check if this instruction updates knowledge.
        update_knowledge_from_instr(&body[i], known);

        i += 1;
    }
    changed
}

/// Update field knowledge from an instruction (after processing its children).
fn update_knowledge_from_instr(instr: &WirInstr, known: &mut FieldKnowledge<'_>) {
    match instr {
        // LocalSet with StructNew: record known fields
        WirInstr::LocalSet { name, value } => {
            // First invalidate any existing knowledge for this local
            known.invalidate_local(name);
            match value.as_ref() {
                // Direct StructNew: record known fields
                WirInstr::StructNew { type_id, fields } => {
                    known.record_struct_new(name, type_id.clone(), fields);
                }
                // Block whose result is a LocalGet: copy knowledge from that local
                WirInstr::Block { body, .. } => {
                    if let Some(source_name) = extract_block_result_local(body) {
                        copy_field_knowledge(known, &source_name, name);
                    }
                }
                // LocalGet: copy knowledge from source local
                WirInstr::LocalGet { name: source } => {
                    copy_field_knowledge(known, source, name);
                }
                // ValueCopy of a LocalGet: copy knowledge
                WirInstr::ValueCopy { expr, .. } => {
                    if let WirInstr::LocalGet { name: source } = expr.as_ref() {
                        copy_field_knowledge(known, source, name);
                    }
                }
                _ => {}
            }
        }
        // StructSet: invalidate the specific field
        WirInstr::StructSet {
            field_name, expr, ..
        } => {
            if let WirInstr::LocalGet { name } = expr.as_ref() {
                known.invalidate_field(name, field_name);
            }
        }
        // Function calls: invalidate all locals passed as arguments
        // (they could be modified via &mut references)
        WirInstr::Call { args, .. } | WirInstr::CallRef { args, .. } => {
            for arg in args {
                match arg {
                    WirInstr::LocalGet { name } => {
                        known.invalidate_local(name);
                    }
                    WirInstr::RefAsNonNull(inner) => {
                        if let WirInstr::LocalGet { name } = inner.as_ref() {
                            known.invalidate_local(name);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Process a single instruction, recursively forwarding constants.
/// Returns true if any changes were made.
fn forward_fields_in_instr(instr: &mut WirInstr, known: &mut FieldKnowledge<'_>) -> bool {
    let mut changed = false;

    match instr {
        // Recurse into block bodies
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            changed |= forward_fields_in_body(body, known);
        }
        WirInstr::Loop { body, .. } => {
            // Conservatively invalidate all knowledge for loops
            // (locals could be modified on back-edges)
            let mut loop_known = FieldKnowledge::new(known.types, known.aliased);
            changed |= forward_fields_in_body(body, &mut loop_known);
            // Invalidate outer knowledge for locals modified inside the loop
            invalidate_locals_modified_in_body(body, known);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            // Forward in the condition
            changed |= forward_fields_in_instr(condition, known);

            // Forward into branches with cloned knowledge
            let mut then_known = FieldKnowledge {
                fields: known.fields.clone(),
                types: known.types,
                aliased: known.aliased,
            };
            changed |= forward_fields_in_body(then_body, &mut then_known);
            if let Some(eb) = else_body {
                let mut else_known = FieldKnowledge {
                    fields: known.fields.clone(),
                    types: known.types,
                    aliased: known.aliased,
                };
                changed |= forward_fields_in_body(eb, &mut else_known);
            }
            // Conservatively invalidate locals modified in branches
            invalidate_locals_modified_in_body(then_body, known);
            if let Some(eb) = else_body {
                invalidate_locals_modified_in_body(eb, known);
            }
        }
        _ => {
            // For other instructions, try to forward StructGet(LocalGet(x), field)
            changed |= try_forward_struct_gets(instr, known);
            // Recurse into children
            instr.for_each_boxed_child_mut(&mut |child| {
                changed |= forward_fields_in_instr(child, known);
            });
        }
    }

    changed
}

/// Try to replace `StructGet(LocalGet(x), field)` with a known constant.
fn try_forward_struct_gets(instr: &mut WirInstr, known: &FieldKnowledge<'_>) -> bool {
    if let WirInstr::StructGet {
        field_name, expr, ..
    } = instr
        && let WirInstr::LocalGet { name } = expr.as_ref()
        && let Some(const_val) = known.get(name, field_name)
    {
        *instr = const_val.clone();
        return true;
    }
    false
}

/// Invalidate field knowledge for any locals that are assigned in a body.
/// Extract the local name from a block's result value.
/// Matches patterns like: `[..., LocalGet { name }, Br { depth: 0 }]`
/// or `[..., Seq([LocalGet { name }, Br { depth: 0 }])]`.
fn extract_block_result_local(body: &[WirInstr]) -> Option<String> {
    // Check last instruction(s) for LocalGet + Br pattern
    let len = body.len();
    if len >= 2
        && let WirInstr::Br { depth: 0 } = &body[len - 1]
        && let WirInstr::LocalGet { name } = &body[len - 2]
    {
        return Some(name.clone());
    }
    // Check for Seq([LocalGet, Br]) as the last instruction
    if let Some(WirInstr::Seq(seq)) = body.last()
        && seq.len() >= 2
    {
        let slen = seq.len();
        if let WirInstr::Br { depth: 0 } = &seq[slen - 1]
            && let WirInstr::LocalGet { name } = &seq[slen - 2]
        {
            return Some(name.clone());
        }
    }
    None
}

/// Copy all known field values from one local to another.
fn copy_field_knowledge(known: &mut FieldKnowledge<'_>, from: &str, to: &str) {
    if known.aliased.contains(to) {
        return;
    }
    let entries: Vec<(String, WirInstr)> = known
        .fields
        .iter()
        .filter(|((name, _), _)| name == from)
        .map(|((_, field), val)| (field.clone(), val.clone()))
        .collect();
    for (field, val) in entries {
        known.fields.insert((to.to_string(), field), val);
    }
}

fn invalidate_locals_modified_in_body(body: &[WirInstr], known: &mut FieldKnowledge<'_>) {
    for instr in body {
        invalidate_locals_modified_in_instr(instr, known);
    }
}

fn invalidate_locals_modified_in_instr(instr: &WirInstr, known: &mut FieldKnowledge<'_>) {
    match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
            known.invalidate_local(name);
        }
        WirInstr::StructSet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name } = expr.as_ref() {
                known.invalidate_field(name, field_name);
            }
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| {
        invalidate_locals_modified_in_instr(child, known);
    });
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

