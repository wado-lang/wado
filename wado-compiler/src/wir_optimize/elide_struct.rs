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
                // Same safety check as the single-field pass.
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
        // Function-wide def/use counts.
        let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
        let mut dummy_candidates: IndexMap<String, Candidate> = IndexMap::default();
        for instr in body.iter() {
            collect_stats(instr, &mut stats, &mut dummy_candidates, 1, 1);
        }
        // Run until a fixed point: each successful elision may expose a new
        // adjacent pair when the use was wrapping another candidate.
        while elide_adjacent_in_body(body, &stats) {}
    }
}

/// Recurse into nested block bodies and process adjacent pairs at each level.
/// Returns `true` if any elision happened.
fn elide_adjacent_in_body(body: &mut Vec<WirInstr>, stats: &IndexMap<String, LocalStats>) -> bool {
    let mut changed = false;
    // First, recurse into nested bodies.
    for instr in body.iter_mut() {
        changed |= elide_adjacent_in_nested(instr, stats);
    }
    // Then process adjacent pairs at this level. We skip over `Nop`
    // placeholders (left behind by earlier elision passes) when locating the
    // "next" sibling so that `LocalSet; nop; nop; use` is treated identically
    // to `LocalSet; use`.
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
            // The condition is a single expression that may itself be a Block
            // wrapping the candidate pair (e.g. `__inline_..._block -> bool { LocalSet; break; }`).
            changed |= elide_adjacent_in_nested(condition, stats);
            changed |= elide_adjacent_in_body(then_body, stats);
            if let Some(eb) = else_body {
                changed |= elide_adjacent_in_body(eb, stats);
            }
        }
        _ => {
            // Generic recursion: look for nested bodies inside any other
            // expression carrier (e.g. `LocalSet { value: Block { body } }`,
            // `Call { args: [..., Block { body }, ...] }`).
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
    // Match: body[i] = LocalSet { name, value: StructNew { fields: [_inner] } }
    let (name, _struct_type, _field_count) = match &body[i] {
        WirInstr::LocalSet { name, value } => match value.as_ref() {
            WirInstr::StructNew { type_id, fields } if fields.len() == 1 => {
                (name.clone(), type_id.clone(), 1usize)
            }
            _ => return false,
        },
        _ => return false,
    };
    // Local must have exactly one def and one StructGet use across the function,
    // and nothing else.
    let s = match stats.get(&name) {
        Some(s) => s,
        None => return false,
    };
    if s.defs != 1 || s.structget_uses != 1 || s.total_localgets != 1 {
        return false;
    }
    // Discover the field name (the only entry in field_uses).
    if s.field_uses.len() != 1 {
        return false;
    }
    let field_name = s.field_uses.keys().next().unwrap().clone();
    // The use must be the leftmost-evaluated descendant of body[j].
    if !use_is_leftmost(&body[j], &name, &field_name) {
        return false;
    }
    // All checks passed. Take the inner initializer and substitute it at the
    // StructGet position; nop the LocalSet.
    let inner = match std::mem::replace(&mut body[i], WirInstr::Nop) {
        WirInstr::LocalSet { value, .. } => match *value {
            WirInstr::StructNew { mut fields, .. } => fields.remove(0),
            other => {
                // Restore — should be unreachable per the match above.
                body[i] = WirInstr::LocalSet {
                    name: name.clone(),
                    value: Box::new(other),
                };
                return false;
            }
        },
        other => {
            body[i] = other;
            return false;
        }
    };
    substitute_first_use(&mut body[j], &name, &field_name, inner);
    true
}

/// Walk `instr` in evaluation order. Returns `true` iff the very first
/// "observable" sub-expression encountered is `StructGet(LocalGet(name), field)`
/// for the given `name` and `field`. Side-effect-free containers (`LocalGet`,
/// constants, arithmetic, ref ops, struct/array reads, …) are walked through.
/// Side-effecting containers (Call, `StructSet`, `ArraySet`, `GlobalSet`, `LocalSet`,
/// stores, control flow) abort the walk before the target.
fn use_is_leftmost(instr: &WirInstr, name: &str, field_name: &str) -> bool {
    matches!(
        walk_for_leftmost(instr, name, field_name),
        LeftmostWalk::Found
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftmostWalk {
    /// Found the target `StructGet` first (in eval order).
    Found,
    /// Walked through pure code; target not encountered.
    Pure,
    /// Hit a side-effecting op or control-flow boundary before the target.
    Blocked,
}

fn walk_for_leftmost(instr: &WirInstr, name: &str, field_name: &str) -> LeftmostWalk {
    // Direct match on the target.
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
    // Containers / opaque control flow that prevent us from peering past them.
    match instr {
        // Loop bodies execute conditionally (back-edges); If's branches are
        // conditional. Don't try to look through these.
        WirInstr::Loop { .. } | WirInstr::If { .. } => LeftmostWalk::Blocked,
        // Control-flow exits occur after their (optional) value child.
        WirInstr::Br { .. }
        | WirInstr::BrIf { .. }
        | WirInstr::BrTable { .. }
        | WirInstr::Return { .. }
        | WirInstr::Unreachable => LeftmostWalk::Blocked,
        // Side-effecting roots.
        WirInstr::Call { .. }
        | WirInstr::CallIndirect { .. }
        | WirInstr::CallRef { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. }
        | WirInstr::ArrayCopy { .. }
        | WirInstr::ArrayFill { .. }
        | WirInstr::GlobalSet { .. }
        | WirInstr::LocalSet { .. }
        | WirInstr::LocalTee { .. }
        | WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. }
        | WirInstr::V128Store { .. }
        | WirInstr::MemoryGrow(_)
        | WirInstr::MemoryFill { .. }
        | WirInstr::MultiValueLocalBind { .. } => {
            // Walking the children first might still find the target before the
            // root side effect fires — that's fine, but if children are pure we
            // must report Blocked because the root is the next observable op.
            let result = walk_children_for_leftmost(instr, name, field_name);
            if matches!(result, LeftmostWalk::Found) {
                LeftmostWalk::Found
            } else {
                LeftmostWalk::Blocked
            }
        }
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

/// Replace the first encountered `StructGet(LocalGet(name), field_name)` in
/// `instr`'s tree with `replacement` (consuming it). Walks in eval order so
/// the pre-validated leftmost match is the one that gets rewritten.
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
