//! Struct field constant forwarding for WIR.
//!
//! Per-function pass that propagates known constant field values through `StructGet`,
//! folds constant comparisons, and eliminates dead branches.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirFunction, WirInstr, WirPackage, WirTypeDef, WirTypeId};

use super::util::collect_local_gets_deep;

pub(super) fn forward_struct_field_constants(module: &mut WirPackage) {
    let types = &module.types;
    for func_idx in 0..module.functions.len() {
        let Some(body) = module.functions[func_idx].body.take() else {
            continue;
        };
        // Collect locals whose references escape. Field forwarding is unsafe
        // for these locals because their fields can be modified through aliases.
        // Uses stores info: locals passed to functions without `stores` for that
        // parameter are NOT marked as aliased.
        let aliased = collect_aliased_locals(&body, &module.functions);
        let mut body = body;
        let mut changed = true;
        while changed {
            let mut known = FieldKnowledge::new(types, &aliased);
            changed = forward_fields_in_body(&mut body, &mut known);
        }
        module.functions[func_idx].body = Some(body);
    }
}

/// Collect locals whose references escape (address taken, embedded in structs,
/// or passed to function calls that declare `stores` for that parameter).
/// Locals passed to functions without `stores` are NOT aliased — the callee
/// cannot retain the reference beyond the call.
fn collect_aliased_locals(body: &[WirInstr], functions: &[WirFunction]) -> IndexSet<String> {
    let mut aliased = IndexSet::default();
    for instr in body {
        collect_aliased_in_instr(instr, &mut aliased, functions, false);
    }
    aliased
}

/// Recursively collect aliased locals.
///
/// `in_non_stores_arg`: when true, we are inside a Call argument whose callee
/// does not declare `stores` for this parameter position. In this context,
/// `RefAsNonNull(LocalGet)` and bare `LocalGet` do not create persistent aliases.
fn collect_aliased_in_instr(
    instr: &WirInstr,
    aliased: &mut IndexSet<String>,
    functions: &[WirFunction],
    in_non_stores_arg: bool,
) {
    match instr {
        // Direct function calls: check stores for each parameter.
        WirInstr::Call { func_id, args } => {
            let callee = functions.get(func_id.index() as usize);
            for (i, arg) in args.iter().enumerate() {
                let stores_param = callee
                    .and_then(|f| f.param_names.get(i))
                    .map(|name| f_stores_param(f_stores(callee), name))
                    .unwrap_or(true);
                if stores_param {
                    // Callee may store this reference — mark all locals as aliased.
                    collect_local_gets_deep(arg, aliased);
                }
                // Recurse into sub-expressions (nested calls get their own analysis).
                collect_aliased_in_instr(arg, aliased, functions, !stores_param);
            }
            return; // Skip default for_each_child — args handled above.
        }
        // Indirect calls: conservative (unknown callee).
        WirInstr::CallRef { args, .. } => {
            for arg in args {
                collect_local_gets_deep(arg, aliased);
                collect_aliased_in_instr(arg, aliased, functions, false);
            }
            return;
        }
        // RefAsNonNull of a LocalGet: address taken — but suppress if inside
        // a non-stores call argument (the reference doesn't persist).
        WirInstr::RefAsNonNull(inner) => {
            if !in_non_stores_arg && let WirInstr::LocalGet { name, .. } = inner.as_ref() {
                aliased.insert(name.clone());
            }
        }
        // LocalSet from another local: both are aliases of the same GC object.
        WirInstr::LocalSet { name, value } => {
            if let WirInstr::LocalGet { name: source, .. } = value.as_ref() {
                aliased.insert(name.clone());
                aliased.insert(source.clone());
            }
        }
        _ => {}
    }
    // Recurse into children, propagating the suppression context.
    instr.for_each_child(&mut |child| {
        collect_aliased_in_instr(child, aliased, functions, in_non_stores_arg);
    });
}

fn f_stores(callee: Option<&WirFunction>) -> &[String] {
    callee.map(|f| f.stores.as_slice()).unwrap_or(&[])
}

fn f_stores_param(stores: &[String], param_name: &str) -> bool {
    stores.iter().any(|s| s == param_name)
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

    /// Record known fields from a `StructNew` assigned to `local_name`.
    /// Records both constant values and `LocalGet` references.
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
                && is_forwardable(field_instr)
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
    /// Also invalidates entries whose stored value is a `LocalGet` referencing
    /// the reassigned local, since that value is no longer valid.
    fn invalidate_local(&mut self, local_name: &str) {
        self.fields.retain(|(name, _), val| {
            if name == local_name {
                return false;
            }
            // If the stored value references the reassigned local, invalidate it
            if let WirInstr::LocalGet { name: source, .. } = val
                && source == local_name
            {
                return false;
            }
            true
        });
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

/// Check if a WIR instruction is forwardable through struct fields.
/// Includes constants and `LocalGet` (variable references).
fn is_forwardable(instr: &WirInstr) -> bool {
    is_wir_constant(instr) || matches!(instr, WirInstr::LocalGet { .. })
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
                // Block whose result is a StructNew: record its fields
                WirInstr::Block { body, .. } => {
                    if let Some(source_name) = extract_block_result_local(body) {
                        copy_field_knowledge(known, &source_name, name);
                    } else if let Some((type_id, fields)) = extract_block_result_struct_new(body) {
                        known.record_struct_new(name, type_id, &fields);
                    }
                }
                // LocalGet: copy knowledge from source local
                WirInstr::LocalGet { name: source, .. } => {
                    copy_field_knowledge(known, source, name);
                }
                _ => {}
            }
        }
        // StructSet: invalidate the specific field
        WirInstr::StructSet {
            field_name, expr, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                known.invalidate_field(name, field_name);
            }
        }
        // Function calls: invalidate all locals passed as arguments
        // (they could be modified via &mut references)
        WirInstr::Call { args, .. } | WirInstr::CallRef { args, .. } => {
            for arg in args {
                match arg {
                    WirInstr::LocalGet { name, .. } => {
                        known.invalidate_local(name);
                    }
                    WirInstr::RefAsNonNull(inner) => {
                        if let WirInstr::LocalGet { name, .. } = inner.as_ref() {
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
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && let Some(const_val) = known.get(name, field_name)
    {
        *instr = const_val.clone();
        return true;
    }
    false
}

/// Return the slice with any trailing `Unreachable` instructions removed.
fn skip_trailing_unreachable(body: &[WirInstr]) -> &[WirInstr] {
    let mut end = body.len();
    while end > 0 && matches!(body[end - 1], WirInstr::Unreachable) {
        end -= 1;
    }
    &body[..end]
}

/// Invalidate field knowledge for any locals that are assigned in a body.
/// Extract the local name from a block's result value.
/// Matches patterns like: `[..., LocalGet { name }, Br { depth: 0 }]`
/// or `[..., Seq([LocalGet { name }, Br { depth: 0 }])]`.
///
/// Trailing `Unreachable` instructions are ignored — the code generator
/// may append `unreachable` after a `Br` so the Wasm validator accepts
/// the typed block even when the fallthrough path is dead.
fn extract_block_result_local(body: &[WirInstr]) -> Option<String> {
    let body = skip_trailing_unreachable(body);
    // Check last instruction(s) for LocalGet + Br pattern
    let len = body.len();
    if len >= 2
        && let WirInstr::Br { depth: 0 } = &body[len - 1]
        && let WirInstr::LocalGet { name, .. } = &body[len - 2]
    {
        return Some(name.clone());
    }
    // Check for Seq([LocalGet, Br]) as the last instruction
    if let Some(WirInstr::Seq(seq)) = body.last()
        && seq.len() >= 2
    {
        let slen = seq.len();
        if let WirInstr::Br { depth: 0 } = &seq[slen - 1]
            && let WirInstr::LocalGet { name, .. } = &seq[slen - 2]
        {
            return Some(name.clone());
        }
    }
    None
}

/// Extract a `StructNew` from a block's result value.
/// Matches patterns like: `[..., StructNew { ... }, Br { depth: 0 }]`
/// or `[..., Seq([StructNew { ... }, Br { depth: 0 }])]`.
/// Returns the (`type_id`, fields) cloned from the `StructNew`.
///
/// Only safe when the block has a single exit point (one `Br { depth: 0 }`).
/// Blocks with multiple exits (e.g., early `break` inside branches) may
/// produce different `StructNew` values on different paths.
///
/// Trailing `Unreachable` instructions are ignored — the code generator
/// may append `unreachable` after a `Br` so the Wasm validator accepts
/// the typed block even when the fallthrough path is dead.
fn extract_block_result_struct_new(body: &[WirInstr]) -> Option<(WirTypeId, Vec<WirInstr>)> {
    // Count Br { depth: 0 } in the block body. If there are multiple, the
    // block result is ambiguous and we cannot safely forward fields.
    if count_br_depth_zero(body) != 1 {
        return None;
    }

    let extract = |items: &[WirInstr]| -> Option<(WirTypeId, Vec<WirInstr>)> {
        let items = skip_trailing_unreachable(items);
        let len = items.len();
        if len >= 2
            && let WirInstr::Br { depth: 0 } = &items[len - 1]
            && let WirInstr::StructNew { type_id, fields } = &items[len - 2]
        {
            return Some((type_id.clone(), fields.clone()));
        }
        None
    };

    if let Some(result) = extract(body) {
        return Some(result);
    }
    let body = skip_trailing_unreachable(body);
    if let Some(WirInstr::Seq(seq)) = body.last() {
        return extract(seq);
    }
    None
}

/// Count the number of `Br { depth: 0 }` instructions in a block body,
/// adjusting depth when entering nested blocks/loops.
fn count_br_depth_zero(body: &[WirInstr]) -> usize {
    let mut count = 0;
    for instr in body {
        count_br_depth_zero_in_instr(instr, 0, &mut count);
    }
    count
}

fn count_br_depth_zero_in_instr(instr: &WirInstr, nesting: u32, count: &mut usize) {
    match instr {
        WirInstr::Br { depth } => {
            if *depth == nesting {
                *count += 1;
            }
        }
        WirInstr::Block { body, .. } => {
            for child in body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
        }
        WirInstr::Loop { body, .. } => {
            // Loop `Br { depth: 0 }` targets the loop itself (continue),
            // not the outer block. Increment nesting so those don't count.
            for child in body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            // Condition is evaluated outside the if label scope
            count_br_depth_zero_in_instr(condition, nesting, count);
            // If body creates a Wasm label: Br { depth: 0 } inside targets
            // the if, not the enclosing block. Increment nesting.
            for child in then_body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
            if let Some(eb) = else_body {
                for child in eb {
                    count_br_depth_zero_in_instr(child, nesting + 1, count);
                }
            }
        }
        WirInstr::Seq(body) => {
            for child in body {
                count_br_depth_zero_in_instr(child, nesting, count);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                count_br_depth_zero_in_instr(child, nesting, count);
            });
        }
    }
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
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                known.invalidate_field(name, field_name);
            }
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| {
        invalidate_locals_modified_in_instr(child, known);
    });
}
