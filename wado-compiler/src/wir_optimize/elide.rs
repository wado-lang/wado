//! Struct local elimination and write-only local cleanup passes for WIR.
//!
//! - **Dead single-field struct local elimination**: substitutes field access directly.
//! - **Dead multi-field struct local elimination**: substitutes each field access once.
//! - **Flatten seq assignments**: canonicalizes LocalSet(x, Seq([preamble, final])).
//! - **Write-only local elimination**: drops LocalSet(x, expr) when x is never read.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirModule, WirTypeDef};

use super::{collect_local_gets_deep, is_side_effect_free};

pub(super) fn elide_dead_single_field_struct_locals(module: &mut WirModule) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        while elide_struct_locals_one_pass(body) {}
    }
}

/// Whole-module pass: elide struct-typed locals for N-field structs (N > 1).
///
/// Handles patterns where a local is set from a StructNew with N fields and each
/// field is accessed exactly once via StructGet. Substitutes each field access
/// with the corresponding initializer expression and nops the original assignment.
/// This eliminates the struct type from the binary.
pub(super) fn elide_dead_multi_field_struct_locals(module: &mut WirModule) {
    // Collect type info needed before mutably borrowing functions.
    let type_field_names: Vec<Option<Vec<String>>> = module
        .types
        .iter()
        .map(|t| match t {
            WirTypeDef::Struct(s) => Some(s.fields.iter().map(|f| f.name.clone()).collect()),
            _ => None,
        })
        .collect();
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        while elide_multi_field_struct_locals_one_pass(body, &type_field_names) {}
    }
}

/// One pass for multi-field struct local elision. Returns `true` if anything changed.
/// `type_field_names[i]` is `Some(vec_of_field_names)` if type i is a struct, else `None`.
fn elide_multi_field_struct_locals_one_pass(
    body: &mut [WirInstr],
    type_field_names: &[Option<Vec<String>>],
) -> bool {
    // Step 1: collect LocalSet(name, StructNew { fields: [e0..eN-1] }) for N > 1.
    // Maps name -> (type_idx, fields).
    let mut all_defs: IndexMap<String, (usize, Vec<WirInstr>)> = IndexMap::default();
    for instr in body.iter() {
        collect_multi_field_struct_defs(instr, &mut all_defs);
    }
    if all_defs.is_empty() {
        return false;
    }

    let candidate_names: IndexSet<String> = all_defs.keys().cloned().collect();

    // Step 2: filter to valid candidates.
    //   - Exactly one LocalSet/LocalTee of this name
    //   - Every LocalGet(name) is the direct expr of a StructGet
    //   - Each field is accessed exactly once via StructGet(LocalGet(name), field_k)
    //   - All N fields are accessed (total uses == N)
    //   - Inner field values don't reference other candidates
    let valid: IndexMap<String, IndexMap<String, WirInstr>> = all_defs
        .into_iter()
        .filter_map(|(name, (type_idx, fields))| {
            // Must have more than 1 field (single-field handled by other pass).
            if fields.len() <= 1 {
                return None;
            }
            // Exactly one def.
            let def_count: usize = body.iter().map(|i| count_local_defs(i, &name)).sum();
            if def_count != 1 {
                return None;
            }
            // All LocalGet(name) are via StructGet.
            if !body.iter().all(|i| local_used_only_via_struct_get(i, &name)) {
                return None;
            }
            // Get field names from pre-computed table.
            let struct_field_names = type_field_names.get(type_idx)?.as_ref()?;
            if struct_field_names.len() != fields.len() {
                return None;
            }
            // Total StructGet uses must equal number of fields.
            let use_count: usize = body.iter().map(|i| count_struct_get_uses(i, &name)).sum();
            if use_count != fields.len() {
                return None;
            }
            // Each field must be accessed exactly once.
            for field_name in struct_field_names {
                let field_uses: usize = body
                    .iter()
                    .map(|i| count_struct_get_field_uses(i, &name, field_name))
                    .sum();
                if field_uses != 1 {
                    return None;
                }
            }
            // No inner value references other candidates.
            for inner in &fields {
                if inner_refs_any_candidate(inner, &candidate_names, &name) {
                    return None;
                }
            }
            // Build substitution map: field_name -> fields[k].
            let subst: IndexMap<String, WirInstr> = struct_field_names
                .iter()
                .zip(fields)
                .map(|(f, v)| (f.clone(), v))
                .collect();
            Some((name, subst))
        })
        .collect();

    if valid.is_empty() {
        return false;
    }

    // Step 3: substitute and nop.
    for (name, field_map) in &valid {
        for instr in body.iter_mut() {
            substitute_multi_field_struct_get(instr, name, field_map);
            nop_local_set_of(instr, name);
        }
    }
    true
}

/// Recursively collect `LocalSet(name, StructNew { fields: [e0..eN-1] })` for N > 1.
fn collect_multi_field_struct_defs(
    instr: &WirInstr,
    defs: &mut IndexMap<String, (usize, Vec<WirInstr>)>,
) {
    if let WirInstr::LocalSet { name, value } = instr
        && let WirInstr::StructNew { type_id, fields } = value.as_ref()
        && fields.len() > 1
    {
        defs.insert(name.clone(), (type_id.index() as usize, fields.clone()));
    }
    instr.for_each_child(&mut |child| collect_multi_field_struct_defs(child, defs));
}

/// Count `StructGet(LocalGet(name), field_name)` for a specific field_name.
fn count_struct_get_field_uses(instr: &WirInstr, local_name: &str, field_name: &str) -> usize {
    if let WirInstr::StructGet { field_name: f, expr, .. } = instr
        && let WirInstr::LocalGet { name: n } = expr.as_ref()
        && n == local_name
        && f == field_name
    {
        return 1;
    }
    let mut count = 0usize;
    instr.for_each_child(&mut |child| {
        count += count_struct_get_field_uses(child, local_name, field_name);
    });
    count
}

/// Replace `StructGet(LocalGet(name), field_name)` with the mapped substitution value.
fn substitute_multi_field_struct_get(
    instr: &mut WirInstr,
    local_name: &str,
    field_map: &IndexMap<String, WirInstr>,
) {
    if let WirInstr::StructGet { field_name, expr, .. } = instr
        && let WirInstr::LocalGet { name: n } = expr.as_ref()
        && n == local_name
    {
        if let Some(value) = field_map.get(field_name) {
            *instr = value.clone();
            return;
        }
    }
    instr
        .for_each_boxed_child_mut(&mut |child| substitute_multi_field_struct_get(child, local_name, field_map));
}

/// One pass: collect candidates, validate, rewrite. Returns `true` if anything changed.
fn elide_struct_locals_one_pass(body: &mut [WirInstr]) -> bool {
    // Step 1: collect LocalSet(name, StructNew { [inner] }) at any depth.
    let mut all_defs: IndexMap<String, WirInstr> = IndexMap::default();
    for instr in body.iter() {
        collect_struct_single_field_defs(instr, &mut all_defs);
    }
    if all_defs.is_empty() {
        return false;
    }

    // Names of all candidates (used for leaf check).
    let candidate_names: IndexSet<String> = all_defs.keys().cloned().collect();

    // Step 2: filter to valid leaf candidates.
    //   - exactly one LocalSet/LocalTee of this name in the whole tree
    //   - every LocalGet(name) is the direct expr of a StructGet
    //   - exactly one StructGet use (safe to inline inner without duplicating effects)
    //   - inner value does not reference any other candidate (process leaves first)
    let valid: IndexMap<String, WirInstr> = all_defs
        .into_iter()
        .filter(|(name, inner)| {
            let def_count: usize = body.iter().map(|i| count_local_defs(i, name)).sum();
            if def_count != 1 {
                return false;
            }
            if !body.iter().all(|i| local_used_only_via_struct_get(i, name)) {
                return false;
            }
            let use_count: usize = body.iter().map(|i| count_struct_get_uses(i, name)).sum();
            if use_count != 1 {
                return false;
            }
            !inner_refs_any_candidate(inner, &candidate_names, name)
        })
        .collect();

    if valid.is_empty() {
        return false;
    }

    // Step 3: substitute inner at StructGet use sites, nop the defining LocalSet.
    for (name, inner) in &valid {
        for instr in body.iter_mut() {
            substitute_struct_get_local(instr, name, inner);
            nop_local_set_of(instr, name);
        }
    }
    true
}

/// Recursively collect `LocalSet(name, StructNew { [inner] })` at any depth,
/// including inside `Call` args and nested block bodies.
fn collect_struct_single_field_defs(instr: &WirInstr, defs: &mut IndexMap<String, WirInstr>) {
    if let WirInstr::LocalSet { name, value } = instr
        && let WirInstr::StructNew { fields, .. } = value.as_ref()
        && fields.len() == 1
    {
        defs.insert(name.clone(), fields[0].clone());
    }
    instr.for_each_child(&mut |child| collect_struct_single_field_defs(child, defs));
}

/// Count `LocalSet(name, ..)` and `LocalTee(name, ..)` occurrences at any depth.
fn count_local_defs(instr: &WirInstr, name: &str) -> usize {
    let self_count = usize::from(matches!(
        instr,
        WirInstr::LocalSet { name: n, .. } | WirInstr::LocalTee { name: n, .. } if n == name
    ));
    let mut child_count = 0usize;
    instr.for_each_child(&mut |child| {
        child_count += count_local_defs(child, name);
    });
    self_count + child_count
}

/// Count `StructGet(LocalGet(name), _)` occurrences at any depth.
fn count_struct_get_uses(instr: &WirInstr, name: &str) -> usize {
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::LocalGet { name: n } = expr.as_ref()
        && n == name
    {
        return 1;
    }
    let mut count = 0usize;
    instr.for_each_child(&mut |child| {
        count += count_struct_get_uses(child, name);
    });
    count
}

/// Returns `true` if every `LocalGet(name)` in the tree is the direct `expr`
/// of a `StructGet` (any field).  A bare `LocalGet(name)` in any other position
/// returns `false`.
fn local_used_only_via_struct_get(instr: &WirInstr, name: &str) -> bool {
    match instr {
        // Bare LocalGet of our name — invalid.
        WirInstr::LocalGet { name: n } => n != name,
        // StructGet whose expr IS exactly LocalGet(name) — valid; don't recurse into expr.
        WirInstr::StructGet { expr, .. } if matches!(expr.as_ref(), WirInstr::LocalGet { name: n } if n == name) => {
            true
        }
        // All other nodes: check children.
        _ => {
            let mut ok = true;
            instr.for_each_child(&mut |child| {
                if !local_used_only_via_struct_get(child, name) {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Returns `true` if `instr` contains a `LocalGet` of any name that is in
/// `candidates`, excluding `exclude` (the candidate being checked).
fn inner_refs_any_candidate(
    instr: &WirInstr,
    candidates: &IndexSet<String>,
    exclude: &str,
) -> bool {
    if let WirInstr::LocalGet { name } = instr {
        candidates.contains(name) && name != exclude
    } else {
        let mut found = false;
        instr.for_each_child(&mut |child| {
            if inner_refs_any_candidate(child, candidates, exclude) {
                found = true;
            }
        });
        found
    }
}

/// Replace every `StructGet(LocalGet(name), _)` in the tree with a clone of `value`.
fn substitute_struct_get_local(instr: &mut WirInstr, name: &str, value: &WirInstr) {
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::LocalGet { name: n } = expr.as_ref()
        && n == name
    {
        *instr = value.clone();
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute_struct_get_local(child, name, value));
}

/// Replace the first `LocalSet(name, ..)` found in the tree with `Nop`.
fn nop_local_set_of(instr: &mut WirInstr, name: &str) {
    if let WirInstr::LocalSet { name: n, .. } = instr
        && n == name
    {
        *instr = WirInstr::Nop;
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| nop_local_set_of(child, name));
}


/// Flatten `LocalSet { name, value: Seq([preamble..., final]) }` into
/// `[preamble..., LocalSet { name, value: final }]` at all levels of each function.
///
/// This canonicalizes the pattern produced by the WIR builder for tuple destructuring,
/// making it visible to downstream passes like multi-field struct local elision.
pub(super) fn flatten_seq_assignments(module: &mut WirModule) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            flatten_seq_in_body(body);
        }
    }
}

fn flatten_seq_in_body(body: &mut Vec<WirInstr>) {
    // First recurse into nested bodies.
    for instr in body.iter_mut() {
        flatten_seq_in_instr(instr);
    }
    // Then expand any LocalSet { value: Seq([..., final]) } at this level.
    let old = std::mem::take(body);
    for instr in old {
        match instr {
            WirInstr::LocalSet { name, value }
                if matches!(value.as_ref(), WirInstr::Seq(seq) if !seq.is_empty()) =>
            {
                if let WirInstr::Seq(mut seq) = *value {
                    let final_val = seq.pop().unwrap();
                    body.extend(seq);
                    body.push(WirInstr::LocalSet { name, value: Box::new(final_val) });
                }
            }
            other => body.push(other),
        }
    }
}

fn flatten_seq_in_instr(instr: &mut WirInstr) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            flatten_seq_in_body(body);
        }
        WirInstr::If { then_body, else_body, condition, .. } => {
            flatten_seq_in_instr(condition);
            flatten_seq_in_body(then_body);
            if let Some(eb) = else_body {
                flatten_seq_in_body(eb);
            }
        }
        _ => {}
    }
}


pub(super) fn elide_write_only_locals(module: &mut WirModule) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            loop {
                if !elide_write_only_locals_in_body(body) {
                    break;
                }
            }
        }
    }
}

fn elide_write_only_locals_in_body(body: &mut Vec<WirInstr>) -> bool {
    let mut read_locals: IndexSet<String> = IndexSet::default();
    for instr in body.iter() {
        collect_local_gets_deep(instr, &mut read_locals);
    }

    let mut changed = false;
    for instr in body.iter_mut() {
        elide_write_only_in_instr(instr, &read_locals, &mut changed);
    }
    changed
}

fn elide_write_only_in_instr(
    instr: &mut WirInstr,
    read_locals: &IndexSet<String>,
    changed: &mut bool,
) {
    match instr {
        WirInstr::LocalSet { name, value } if !read_locals.contains(name.as_str()) => {
            let value_expr = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if is_side_effect_free(&value_expr) {
                *instr = WirInstr::Nop;
            } else {
                *instr = WirInstr::Drop(Box::new(value_expr));
            }
            *changed = true;
        }
        WirInstr::Block { body, .. }
        | WirInstr::Loop { body, .. }
        | WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                elide_write_only_in_instr(child, read_locals, changed);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter_mut() {
                elide_write_only_in_instr(child, read_locals, changed);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    elide_write_only_in_instr(child, read_locals, changed);
                }
            }
        }
        _ => {}
    }
}
