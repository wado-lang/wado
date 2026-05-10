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

use super::util::is_root_observable;

#[derive(Default)]
struct LocalStats {
    /// Every `LocalGet(name)` anywhere in the tree (including those wrapped in `StructGet`).
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

/// Returns `true` when `instr` is safe to re-evaluate at a different program
/// point (i.e. it is referentially transparent between its definition and any
/// use site).
///
/// Heap reads (`StructGet`, `ArrayGet*`) and calls are rejected because the
/// underlying GC object or array element might have been mutated between the
/// struct-local's definition and its use.  Allocations (`StructNew`, `ArrayNew*`)
/// are rejected because re-allocating creates a distinct object identity.
/// Any side-effecting instruction nested inside an expression is also rejected.
fn is_pure_for_elision(instr: &WirInstr) -> bool {
    match instr {
        // Heap reads: the field/element may have been mutated in the meantime.
        WirInstr::StructGet { .. }
        | WirInstr::ArrayGet { .. }
        | WirInstr::ArrayGetS { .. }
        | WirInstr::ArrayGetU { .. } => false,
        // Function calls: potential side effects.
        WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => false,
        // Heap allocations: re-allocating at the use site creates a new, distinct object.
        WirInstr::StructNew { .. }
        | WirInstr::ArrayNew { .. }
        | WirInstr::ArrayNewDefault { .. }
        | WirInstr::ArrayNewData { .. }
        | WirInstr::ArrayNewFixed { .. } => false,
        // Everything else (constants, LocalGet, arithmetic, comparisons, …):
        // recurse to ensure no impure sub-expressions are present.
        _ => {
            let mut pure = true;
            instr.for_each_child(&mut |child| {
                if pure && !is_pure_for_elision(child) {
                    pure = false;
                }
            });
            pure
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
            // Only substitute when the inner expression is safe to re-evaluate at
            // the use site.  Expressions that read heap state (StructGet, ArrayGet)
            // or have side effects must not be moved past intervening mutations.
            if !is_pure_for_elision(&inner) {
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

    // Filter: candidate is valid when each accessed field is read exactly
    // once and every field initializer is pure. Unaccessed fields are
    // permitted (the partial-use case from `let [_rx, tx] = …`): since
    // every initializer is pure, the unread initialiser just gets dropped
    // along with the eliminated `LocalSet`.
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
            // Every LocalGet(name) must be the source of a StructGet.
            if s.total_localgets != s.structget_uses {
                return None;
            }
            // Each ACCESSED field is read exactly once. Unaccessed fields
            // (count == 0) are fine.
            for fname in struct_field_names {
                let n = s.field_uses.get(fname).copied().unwrap_or(0);
                if n > 1 {
                    return None;
                }
            }
            // The total number of StructGet uses matches the sum of
            // per-field counts (sanity).
            let accessed: u32 = struct_field_names
                .iter()
                .map(|f| s.field_uses.get(f).copied().unwrap_or(0))
                .sum();
            if s.structget_uses != accessed {
                return None;
            }
            for inner in &cand.fields {
                if inner_refs_any_candidate(inner, &candidate_names, &name) {
                    return None;
                }
                // Initializers must be pure so unaccessed ones can be
                // dropped without changing program semantics.
                if !is_pure_for_elision(inner) {
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

/// Whole-module pass: elide single-field struct locals where the only use is
/// the immediately-following sibling instruction and the use site is the
/// leftmost-evaluated descendant of that instruction.
///
/// Unlike [`elide_single_field_struct_locals`], this pass does NOT require the
/// inner field initializer to be re-evaluation safe (`is_pure_for_elision`).
/// Instead, adjacency + leftmost-evaluation ensures that no intervening side
/// effect can mutate state that the inner expression depends on.
///
/// Targets the very common `Box<T>` pattern produced by boxing+inlining:
/// ```text
/// self_N = struct.new "Box<char>" { value: <heap-reading block> };
/// break label: f(self_N.value) == g(...);
/// ```
pub(super) fn elide_adjacent_single_use_struct_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
        for instr in body.iter() {
            collect_local_stats(instr, &mut stats);
        }
        // Stats are computed once and reused across the fixed-point iterations.
        // This is sound because every successful elision (a) replaces the
        // candidate's `LocalSet` and `StructGet` with `Nop` + the inner
        // expression, leaving the candidate's stats stale-but-unused (its name
        // is gone), and (b) preserves def/use counts for every other local —
        // the substitution only relocates the inner expression in the tree.
        while elide_adjacent_in_body(body, &stats) {}
    }
}

/// Stats-only walker — same shape as [`collect_stats`] but skips candidate
/// recording, so the per-`LocalSet` `StructNew` field clone is avoided.
fn collect_local_stats(instr: &WirInstr, stats: &mut IndexMap<String, LocalStats>) {
    match instr {
        WirInstr::LocalGet { name, .. } => {
            stats.entry(name.clone()).or_default().total_localgets += 1;
        }
        WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            collect_local_stats(value, stats);
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                let s = stats.entry(name.clone()).or_default();
                s.total_localgets += 1;
                s.structget_uses += 1;
                *s.field_uses.entry(field_name.clone()).or_insert(0) += 1;
            } else {
                collect_local_stats(expr, stats);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| collect_local_stats(child, stats));
        }
    }
}

fn elide_adjacent_in_body(body: &mut [WirInstr], stats: &IndexMap<String, LocalStats>) -> bool {
    let mut changed = false;
    for instr in body.iter_mut() {
        changed |= elide_adjacent_in_nested(instr, stats);
    }
    // Skip `Nop` placeholders (left by earlier elision passes) when locating
    // the next sibling so `LocalSet; nop; nop; use` is treated identically to
    // `LocalSet; use`.
    let mut i = 0;
    while i < body.len() {
        if let Some(j) = next_non_nop(body, i + 1)
            && try_elide_adjacent_pair(body, i, j, stats)
        {
            changed = true;
        }
        i += 1;
    }
    changed
}

fn next_non_nop(body: &[WirInstr], from: usize) -> Option<usize> {
    (from..body.len()).find(|&k| !matches!(body[k], WirInstr::Nop))
}

fn elide_adjacent_in_nested(instr: &mut WirInstr, stats: &IndexMap<String, LocalStats>) -> bool {
    let mut changed = false;
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            changed |= elide_adjacent_in_body(body, stats);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            changed |= elide_adjacent_in_nested(condition, stats);
            changed |= elide_adjacent_in_body(then_body, stats);
            if let Some(eb) = else_body {
                changed |= elide_adjacent_in_body(eb, stats);
            }
        }
        _ => {
            // The candidate pair lives inside a `Vec<WirInstr>` body, which
            // only `Block`/`Loop`/`Seq`/`If` carry — but those bodies can be
            // wrapped by any expression carrier (e.g. `LocalSet { value: Block }`,
            // `Call { args: [..., Block, ...] }`), so the generic arm walks
            // every boxed child looking for one of those four.
            instr.for_each_boxed_child_mut(&mut |child| {
                changed |= elide_adjacent_in_nested(child, stats);
            });
        }
    }
    changed
}

/// Try to elide `body[i]` (`LocalSet` of single-field `StructNew`) by substituting
/// the sole field initializer into `body[j]` (the next non-Nop sibling) at the
/// `StructGet` position. Returns `true` on success.
fn try_elide_adjacent_pair(
    body: &mut [WirInstr],
    i: usize,
    j: usize,
    stats: &IndexMap<String, LocalStats>,
) -> bool {
    let name = match &body[i] {
        WirInstr::LocalSet { name, value } if matches!(value.as_ref(), WirInstr::StructNew { fields, .. } if fields.len() == 1) => {
            name.clone()
        }
        _ => return false,
    };
    let Some(s) = stats.get(&name) else {
        return false;
    };
    if s.defs != 1 || s.structget_uses != 1 || s.total_localgets != 1 {
        return false;
    }
    if s.field_uses.len() != 1 {
        return false;
    }
    let field_name = s.field_uses.keys().next().unwrap().clone();
    if !use_is_leftmost(&body[j], &name, &field_name) {
        return false;
    }
    let inner = match std::mem::replace(&mut body[i], WirInstr::Nop) {
        WirInstr::LocalSet { value, .. } => match *value {
            WirInstr::StructNew { mut fields, .. } => fields.remove(0),
            _ => unreachable!("guarded by name match above"),
        },
        _ => unreachable!("guarded by name match above"),
    };
    substitute_first_use(&mut body[j], &name, &field_name, inner);
    true
}

/// Walk `instr` in evaluation order. Returns `Found` iff the first observable
/// sub-expression encountered is `StructGet(LocalGet(name), field_name)`.
/// Side-effect-free containers (constants, `LocalGet`, arithmetic, ref ops,
/// struct/array reads, …) are walked through; side-effecting roots (calls,
/// stores, sets) and conditional control flow (`Loop`, `If`) abort the walk.
fn use_is_leftmost(instr: &WirInstr, name: &str, field_name: &str) -> bool {
    matches!(
        walk_for_leftmost(instr, name, field_name),
        LeftmostWalk::Found
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftmostWalk {
    Found,
    Pure,
    Blocked,
}

fn walk_for_leftmost(instr: &WirInstr, name: &str, field_name: &str) -> LeftmostWalk {
    if let WirInstr::StructGet {
        field_name: f,
        expr,
        ..
    } = instr
        && f == field_name
        && let WirInstr::LocalGet { name: n, .. } = expr.as_ref()
        && n == name
    {
        return LeftmostWalk::Found;
    }
    match instr {
        // Conditional control flow: bodies/branches execute conditionally.
        WirInstr::Loop { .. } | WirInstr::If { .. } => LeftmostWalk::Blocked,
        // Observable root ops fire *after* their children, so a target found
        // inside a child arrived first and is safe to elide; otherwise the
        // next observable op is the side effect → Blocked.
        _ if is_root_observable(instr) => match walk_children_for_leftmost(instr, name, field_name)
        {
            LeftmostWalk::Found => LeftmostWalk::Found,
            _ => LeftmostWalk::Blocked,
        },
        _ => walk_children_for_leftmost(instr, name, field_name),
    }
}

fn walk_children_for_leftmost(instr: &WirInstr, name: &str, field_name: &str) -> LeftmostWalk {
    let mut result = LeftmostWalk::Pure;
    let mut stop = false;
    instr.for_each_child(&mut |child| {
        if stop {
            return;
        }
        match walk_for_leftmost(child, name, field_name) {
            LeftmostWalk::Found => {
                result = LeftmostWalk::Found;
                stop = true;
            }
            LeftmostWalk::Blocked => {
                result = LeftmostWalk::Blocked;
                stop = true;
            }
            LeftmostWalk::Pure => {}
        }
    });
    result
}

/// Replace the first `StructGet(LocalGet(name), field_name)` reached in eval
/// order with `replacement` (consumed). The pre-validated leftmost match is
/// guaranteed to be that first hit.
fn substitute_first_use(instr: &mut WirInstr, name: &str, field_name: &str, replacement: WirInstr) {
    let mut slot = Some(replacement);
    do_substitute_first_use(instr, name, field_name, &mut slot);
}

fn do_substitute_first_use(
    instr: &mut WirInstr,
    name: &str,
    field_name: &str,
    slot: &mut Option<WirInstr>,
) {
    if slot.is_none() {
        return;
    }
    if let WirInstr::StructGet {
        field_name: f,
        expr,
        ..
    } = instr
        && f == field_name
        && matches!(expr.as_ref(), WirInstr::LocalGet { name: n, .. } if n == name)
    {
        *instr = slot.take().unwrap();
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        if slot.is_some() {
            do_substitute_first_use(child, name, field_name, slot);
        }
    });
}
