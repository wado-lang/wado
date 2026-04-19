//! Struct local elimination passes for WIR.
//!
//! - **Single-field struct local elimination**: substitutes field access directly
//!   when a local holds a `StructNew { [inner] }` and is only read via `StructGet`.
//! - **Multi-field struct local elimination**: same as above but for structs with
//!   N > 1 fields where each field is accessed exactly once.
//! - **Flatten seq assignments**: canonicalizes `LocalSet(x, Seq([preamble, final]))`.
//!
//! Both elision passes share a single-traversal stats collector that records, per
//! local name: total `LocalGet` count, `StructGet(LocalGet(name), _)` use count,
//! def count (`LocalSet` + `LocalTee`), and per-field use counts. Validation then
//! consults this map in O(C), and substitution walks the body once more. Each
//! iteration of the fixed-point loop is O(N) in body size, replacing the previous
//! per-candidate re-walks (O(C · N)).

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage, WirTypeDef};
use crate::wir_visitor::WirMutVisitor;

#[derive(Default)]
struct LocalStats {
    /// Every `LocalGet(name)` anywhere in the tree (including those wrapped in StructGet).
    total_localgets: u32,
    /// Every `StructGet(LocalGet(name), _)` occurrence.
    structget_uses: u32,
    /// Every `LocalSet(name, _)` or `LocalTee(name, _)` occurrence.
    defs: u32,
    /// Per-field use counts for `StructGet(LocalGet(name), field)`.
    field_uses: IndexMap<String, u32>,
}

/// Candidate def: `LocalSet(name, StructNew { type_idx, fields })`.
struct Candidate {
    type_idx: usize,
    fields: Vec<WirInstr>,
}

pub(super) fn elide_single_field_struct_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        while elide_struct_locals_one_pass(body) {}
    }
}

/// Whole-module pass: elide struct-typed locals for N-field structs (N > 1).
///
/// Handles patterns where a local is set from a `StructNew` with N fields and each
/// field is accessed exactly once via `StructGet`. Substitutes each field access
/// with the corresponding initializer expression and nops the original assignment.
/// This eliminates the struct type from the binary.
pub(super) fn elide_multi_field_struct_locals(module: &mut WirPackage) {
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

/// Single traversal: populate `stats` with def/use counts per local name and,
/// if `record_candidates == Some(min_fields)`, record `LocalSet(name, StructNew)`
/// with at least `min_fields` as candidate defs.
fn collect_stats(
    instr: &WirInstr,
    stats: &mut IndexMap<String, LocalStats>,
    candidates: &mut IndexMap<String, Candidate>,
    min_fields: usize,
    max_fields: usize,
) {
    match instr {
        WirInstr::LocalGet { name, .. } => {
            stats.entry(name.clone()).or_default().total_localgets += 1;
        }
        WirInstr::LocalSet { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            if let WirInstr::StructNew { type_id, fields } = value.as_ref()
                && fields.len() >= min_fields
                && fields.len() <= max_fields
            {
                candidates.insert(
                    name.clone(),
                    Candidate {
                        type_idx: type_id.index() as usize,
                        fields: fields.clone(),
                    },
                );
            }
            collect_stats(value, stats, candidates, min_fields, max_fields);
        }
        WirInstr::LocalTee { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            collect_stats(value, stats, candidates, min_fields, max_fields);
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                let s = stats.entry(name.clone()).or_default();
                s.total_localgets += 1;
                s.structget_uses += 1;
                *s.field_uses.entry(field_name.clone()).or_insert(0) += 1;
                // Don't recurse into expr — it's the LocalGet we just counted.
            } else {
                collect_stats(expr, stats, candidates, min_fields, max_fields);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                collect_stats(child, stats, candidates, min_fields, max_fields);
            });
        }
    }
}

/// Walk `instr` to check whether any descendant `LocalGet(name)` has its name in
/// `candidates` and not equal to `exclude`.
fn inner_refs_any_candidate(
    instr: &WirInstr,
    candidates: &IndexSet<String>,
    exclude: &str,
) -> bool {
    if let WirInstr::LocalGet { name, .. } = instr {
        return candidates.contains(name) && name != exclude;
    }
    let mut found = false;
    instr.for_each_child(&mut |child| {
        if !found && inner_refs_any_candidate(child, candidates, exclude) {
            found = true;
        }
    });
    found
}

/// One pass: collect stats, validate candidates, rewrite. Returns `true` if anything changed.
fn elide_struct_locals_one_pass(body: &mut [WirInstr]) -> bool {
    let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
    let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
    for instr in body.iter() {
        collect_stats(instr, &mut stats, &mut candidates, 1, 1);
    }
    if candidates.is_empty() {
        return false;
    }
    let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();

    // Filter to valid leaf candidates:
    //   - exactly one LocalSet/LocalTee of this name
    //   - exactly one StructGet(LocalGet(name), _) use
    //   - every LocalGet(name) is wrapped by StructGet
    //   - inner value doesn't reference another candidate
    let valid: IndexMap<String, WirInstr> = candidates
        .into_iter()
        .filter_map(|(name, cand)| {
            let s = stats.get(&name)?;
            if s.defs != 1 || s.structget_uses != 1 {
                return None;
            }
            if s.total_localgets != s.structget_uses {
                return None;
            }
            let inner = cand.fields.into_iter().next()?;
            if inner_refs_any_candidate(&inner, &candidate_names, &name) {
                return None;
            }
            Some((name, inner))
        })
        .collect();

    if valid.is_empty() {
        return false;
    }

    for instr in body.iter_mut() {
        substitute_single_field(instr, &valid);
    }
    true
}

/// Single-pass mutator: replaces `StructGet(LocalGet(name), _)` with `inner.clone()`
/// for every valid candidate, and nops their defining `LocalSet`.
fn substitute_single_field(instr: &mut WirInstr, valid: &IndexMap<String, WirInstr>) {
    match instr {
        WirInstr::LocalSet { name, .. } if valid.contains_key(name) => {
            *instr = WirInstr::Nop;
            return;
        }
        WirInstr::StructGet { expr, .. } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref()
                && let Some(inner) = valid.get(name)
            {
                *instr = inner.clone();
                return;
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute_single_field(child, valid));
}

/// One pass for multi-field struct local elision. Returns `true` if anything changed.
/// `type_field_names[i]` is `Some(vec_of_field_names)` if type i is a struct, else `None`.
fn elide_multi_field_struct_locals_one_pass(
    body: &mut [WirInstr],
    type_field_names: &[Option<Vec<String>>],
) -> bool {
    let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
    let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
    for instr in body.iter() {
        collect_stats(instr, &mut stats, &mut candidates, 2, usize::MAX);
    }
    if candidates.is_empty() {
        return false;
    }
    let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();

    // Filter: same as single-field, plus each field accessed exactly once with the
    // struct's declared field names.
    let valid: IndexMap<String, IndexMap<String, WirInstr>> = candidates
        .into_iter()
        .filter_map(|(name, cand)| {
            let s = stats.get(&name)?;
            if s.defs != 1 {
                return None;
            }
            let struct_field_names = type_field_names.get(cand.type_idx)?.as_ref()?;
            if struct_field_names.len() != cand.fields.len() {
                return None;
            }
            if s.structget_uses as usize != cand.fields.len() {
                return None;
            }
            if s.total_localgets != s.structget_uses {
                return None;
            }
            for fname in struct_field_names {
                if s.field_uses.get(fname).copied().unwrap_or(0) != 1 {
                    return None;
                }
            }
            for inner in &cand.fields {
                if inner_refs_any_candidate(inner, &candidate_names, &name) {
                    return None;
                }
            }
            let subst: IndexMap<String, WirInstr> = struct_field_names
                .iter()
                .cloned()
                .zip(cand.fields)
                .collect();
            Some((name, subst))
        })
        .collect();

    if valid.is_empty() {
        return false;
    }

    for instr in body.iter_mut() {
        substitute_multi_field(instr, &valid);
    }
    true
}

/// Single-pass mutator for the multi-field pass.
fn substitute_multi_field(
    instr: &mut WirInstr,
    valid: &IndexMap<String, IndexMap<String, WirInstr>>,
) {
    match instr {
        WirInstr::LocalSet { name, .. } if valid.contains_key(name) => {
            *instr = WirInstr::Nop;
            return;
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref()
                && let Some(field_map) = valid.get(name)
                && let Some(value) = field_map.get(field_name)
            {
                *instr = value.clone();
                return;
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute_multi_field(child, valid));
}

/// Flatten `LocalSet { name, value: Seq([preamble..., final]) }` into
/// `[preamble..., LocalSet { name, value: final }]` at all levels of each function.
///
/// This canonicalizes the pattern produced by the WIR builder for tuple destructuring,
/// making it visible to downstream passes like multi-field struct local elision.
pub(super) fn flatten_seq_assignments(module: &mut WirPackage) {
    let mut visitor = FlattenSeqAssignments;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            visitor.visit_body(body);
        }
    }
}

struct FlattenSeqAssignments;

impl WirMutVisitor for FlattenSeqAssignments {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into nested bodies.
        self.walk_body(body);
        // Then expand any LocalSet { value: Seq([..., final]) } at this level.
        let old = std::mem::take(body);
        for instr in old {
            match instr {
                WirInstr::LocalSet { name, value } if matches!(value.as_ref(), WirInstr::Seq(seq) if !seq.is_empty()) => {
                    if let WirInstr::Seq(mut seq) = *value {
                        let final_val = seq.pop().unwrap();
                        body.extend(seq);
                        body.push(WirInstr::LocalSet {
                            name,
                            value: Box::new(final_val),
                        });
                    }
                }
                other => body.push(other),
            }
        }
    }
}
