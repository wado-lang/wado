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
//!
//! The relaxed adjacent-use variant — `elide_adjacent_single_use_struct_locals`,
//! which used `ModRef::can_move_past` to tolerate intervening unrelated stmts —
//! moved to NIR (`optimize/elide_box_local.rs`). At NIR it consults the
//! alias machinery directly and the freshly substituted expressions feed
//! back into the same fix-point loop where `copy_prop` / `const_fold` /
//! `dce` can fold them further.
//!
//! - **Adjacent-use box elision** (`elide_adjacent_box_locals`): the same
//!   single-field substitution but for a `StructNew{[inner]}` whose `inner`
//!   reads the heap (which `is_pure_for_elision` refuses to move). It is sound
//!   here because it works on ordered statement lists and only fires when every
//!   op evaluated between the def and its single use is pure and unconditional.
//!   This targets the `Box<T>` locals lowering mints for `&primitive` payload
//!   bindings (`match r { Token(i) => f(*i) }`), which never reach NIR — the
//!   boxing scheme (`lower::plan::boxing`) runs during NIR→WIR lowering — so the
//!   NIR `elide_box_local` pass cannot see them.

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
        // Writes: a field initializer can assign a local/global/heap slot as a
        // side effect (e.g. `low: local.tee v(1000)`, where `v` is read
        // elsewhere). Eliding the struct would drop that write — not pure. (A
        // dropped field init must be side-effect-free; an unread field with a
        // write init must keep the struct.)
        WirInstr::LocalSet { .. }
        | WirInstr::LocalTee { .. }
        | WirInstr::GlobalSet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. } => false,
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

// -----------------------------------------------------------------------
// Adjacent-use box elision
// -----------------------------------------------------------------------

/// Result of walking a using statement for the single `StructGet(box)` use in
/// left-to-right evaluation order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoxUseWalk {
    /// The use was reached with only pure, unconditional ops evaluated before it.
    Found,
    /// The whole subtree is pure and contains no use (safe to evaluate the
    /// moved heap read *after* it).
    Pure,
    /// A call, heap write, or conditional/unknown construct is evaluated before
    /// the use (or the use sits in a conditionally-evaluated position). Moving
    /// the heap-read initializer here could change what it reads or whether it
    /// traps.
    Blocked,
}

/// Elide single-field (`Box<T>`) struct locals whose initializer reads the heap,
/// when the def and its single use are adjacent in a straight-line region.
///
/// See the module docs. Runs to a fix-point per function: each iteration
/// recomputes the whole-function def/use `stats` (a read-only oracle) and
/// substitutes every non-overlapping adjacent box into its use.
pub(super) fn elide_adjacent_box_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        loop {
            let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
            let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
            for instr in body.iter() {
                collect_stats(instr, &mut stats, &mut candidates, 1, 1);
            }
            if candidates.is_empty() {
                break;
            }
            let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();
            let mut elider = AdjacentBoxElider {
                stats: &stats,
                candidate_names: &candidate_names,
                changed: false,
            };
            elider.visit_body(body);
            if !elider.changed {
                break;
            }
        }
    }
}

struct AdjacentBoxElider<'a> {
    stats: &'a IndexMap<String, LocalStats>,
    candidate_names: &'a IndexSet<String>,
    changed: bool,
}

impl WirMutVisitor for AdjacentBoxElider<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // Recurse into nested bodies first, then process this statement list.
        self.walk_body(body);
        self.process_stmt_list(body);
    }
}

impl AdjacentBoxElider<'_> {
    fn process_stmt_list(&mut self, stmts: &mut [WirInstr]) {
        for p in 0..stmts.len() {
            let Some((name, field, inner)) = self.describe_box_def(&stmts[p]) else {
                continue;
            };
            let Some(k) = find_adjacent_box_use(stmts, p + 1, &name, &field) else {
                continue;
            };
            if substitute_box_use(&mut stmts[k], &name, &field, &inner) {
                stmts[p] = WirInstr::Nop;
                self.changed = true;
            }
        }
    }

    /// Recognise `LocalSet(name, StructNew{[inner]})` where `name` is defined
    /// once, read exactly once, and that read is a `StructGet(LocalGet(name), f)`.
    /// Returns `(name, field, inner)` — a clone of the single-field initializer.
    fn describe_box_def(&self, stmt: &WirInstr) -> Option<(String, String, WirInstr)> {
        let WirInstr::LocalSet { name, value } = stmt else {
            return None;
        };
        let WirInstr::StructNew { fields, .. } = value.as_ref() else {
            return None;
        };
        if fields.len() != 1 {
            return None;
        }
        let s = self.stats.get(name)?;
        if s.defs != 1 || s.structget_uses != 1 || s.total_localgets != 1 {
            return None;
        }
        if s.field_uses.len() != 1 {
            return None;
        }
        let inner = &fields[0];
        // Never move an initializer that reads another box candidate: the other
        // box may be elided in the same pass, and reordering across it is not
        // covered by the adjacency proof.
        if inner_refs_any_candidate(inner, self.candidate_names, name) {
            return None;
        }
        let field = s.field_uses.keys().next()?.clone();
        Some((name.clone(), field, inner.clone()))
    }
}

/// Find the single `StructGet(LocalGet(name), field)` use. The use must be the
/// leftmost side effect of the *immediately following* non-Nop statement — every
/// op evaluated before it there must be pure and unconditional
/// (`BoxUseWalk::Found`). Restricting to the adjacent statement means the scan
/// never skips over a statement, so no reordering happens across statement
/// boundaries at all — only the (bounded) reorder inside the single using
/// statement, governed by [`leftmost_box_use`].
fn find_adjacent_box_use(
    stmts: &[WirInstr],
    from: usize,
    name: &str,
    field: &str,
) -> Option<usize> {
    let k = (from..stmts.len()).find(|&k| !matches!(stmts[k], WirInstr::Nop))?;
    match leftmost_box_use(&stmts[k], name, field) {
        BoxUseWalk::Found => Some(k),
        BoxUseWalk::Pure | BoxUseWalk::Blocked => None,
    }
}

/// Is `instr` the target `StructGet(LocalGet(name), field)`?
fn is_box_use(instr: &WirInstr, name: &str, field: &str) -> bool {
    matches!(
        instr,
        WirInstr::StructGet { field_name, expr, .. }
            if field_name == field
                && matches!(expr.as_ref(), WirInstr::LocalGet { name: n, .. } if n == name)
    )
}

/// A node whose *own* operation writes memory or performs a call. Its operand
/// children are evaluated (in order) before the effect, so a use found in an
/// operand is still valid; a use reaching past the effect is not.
fn is_effect_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Call { .. }
            | WirInstr::CallIndirect { .. }
            | WirInstr::CallRef { .. }
            | WirInstr::LocalSet { .. }
            | WirInstr::LocalTee { .. }
            | WirInstr::GlobalSet { .. }
            | WirInstr::StructSet { .. }
            | WirInstr::ArraySet { .. }
            | WirInstr::ArrayCopy { .. }
            | WirInstr::ArrayFill { .. }
            | WirInstr::MemoryFill { .. }
            | WirInstr::MemoryGrow(_)
            | WirInstr::TableSet { .. }
            | WirInstr::MultiValueLocalBind { .. }
            | WirInstr::I32Store { .. }
            | WirInstr::I32Store8 { .. }
            | WirInstr::I32Store16 { .. }
            | WirInstr::I64Store { .. }
            | WirInstr::V128Store { .. }
    )
}

/// A node that evaluates children conditionally or transfers control, so the
/// straight-line "leftmost" reasoning does not apply. A use anywhere inside is
/// unsafe to anchor on (it may run conditionally, dropping a trap the
/// unconditional box init would have produced).
fn is_control_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Block { .. }
            | WirInstr::Loop { .. }
            | WirInstr::If { .. }
            | WirInstr::BranchHint { .. }
            | WirInstr::Br { .. }
            | WirInstr::BrIf { .. }
            | WirInstr::BrTable { .. }
            | WirInstr::Return { .. }
            | WirInstr::Unreachable
            | WirInstr::ColdPath
            | WirInstr::Select { .. }
    )
}

/// Walk `instr` in evaluation order looking for the single box use. See
/// [`BoxUseWalk`]. Every effectful / control-flow node is classified as a
/// barrier ([`is_effect_barrier`] / [`is_control_barrier`]); everything else is
/// a pure, unconditional node whose operand children are evaluated left to
/// right (arithmetic, SIMD, reads, casts, allocations, `Drop`, `Seq`, …). The
/// pure-by-default arm is sound only because the barrier lists cover every
/// write, call, and branch; a trap or heap *read* evaluated before the use is
/// harmless, since the moved initializer is a side-effect-free read.
fn leftmost_box_use(instr: &WirInstr, name: &str, field: &str) -> BoxUseWalk {
    if is_box_use(instr, name, field) {
        return BoxUseWalk::Found;
    }
    if is_control_barrier(instr) {
        return BoxUseWalk::Blocked;
    }
    // Walk operand children left to right, short-circuiting once the use is
    // Found or a nested barrier Blocks. `for_each_child` yields children in
    // evaluation order.
    let mut acc = BoxUseWalk::Pure;
    instr.for_each_child(&mut |c| {
        if acc == BoxUseWalk::Pure {
            acc = leftmost_box_use(c, name, field);
        }
    });
    if is_effect_barrier(instr) {
        // Operands ran before the effect: honour a use there, but the effect
        // itself blocks anything reaching past it.
        match acc {
            BoxUseWalk::Found => BoxUseWalk::Found,
            BoxUseWalk::Pure | BoxUseWalk::Blocked => BoxUseWalk::Blocked,
        }
    } else {
        acc
    }
}

/// Replace the first `StructGet(LocalGet(name), field)` in `instr` with `inner`.
/// The candidate has exactly one such use, so the first pre-order match is it.
fn substitute_box_use(instr: &mut WirInstr, name: &str, field: &str, inner: &WirInstr) -> bool {
    if is_box_use(instr, name, field) {
        *instr = inner.clone();
        return true;
    }
    let mut done = false;
    instr.for_each_boxed_child_mut(&mut |child| {
        if !done {
            done = substitute_box_use(child, name, field, inner);
        }
    });
    done
}

#[cfg(test)]
mod adjacent_box_tests {
    use super::*;
    use crate::wir::{WirFuncId, WirType, WirTypeId};
    use std::rc::Rc;

    const NAME: &str = "b";
    const FIELD: &str = "value";

    fn tid() -> WirTypeId {
        WirTypeId::new(0, Rc::from("Box"))
    }
    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    /// The target `StructGet(LocalGet("b"), "value")`.
    fn use_box() -> WirInstr {
        WirInstr::StructGet {
            type_id: tid(),
            field_name: FIELD.to_string(),
            expr: Box::new(lget(NAME)),
            result_ty: WirType::I32,
        }
    }
    fn call(args: Vec<WirInstr>) -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args,
        }
    }
    fn walk(instr: &WirInstr) -> BoxUseWalk {
        leftmost_box_use(instr, NAME, FIELD)
    }

    /// `f(other, *b)` — the use is a later call arg after a pure `LocalGet`.
    /// Found: nothing side-effecting runs before the use.
    #[test]
    fn found_use_after_pure_arg() {
        let instr = call(vec![lget("other"), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `sum + *b` — the use is the right operand of a pure add. Found.
    #[test]
    fn found_use_in_arithmetic() {
        let instr = WirInstr::I32Add(Box::new(lget("sum")), Box::new(use_box()));
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `f(g(), *b)` — a call is evaluated before the use, and it could mutate
    /// what the boxed initializer reads. Blocked.
    #[test]
    fn blocked_by_call_before_use() {
        let instr = call(vec![call(vec![]), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// `*b` nested inside an `If` — the use is conditionally evaluated, so
    /// moving the (possibly-trapping) box init there could drop a trap. Blocked.
    #[test]
    fn blocked_by_conditional() {
        let instr = WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: Some(WirType::I32),
            then_body: vec![use_box()],
            else_body: Some(vec![WirInstr::I32Const(0)]),
        };
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A heap write before the use blocks: the write could change what the
    /// boxed read observes if moved after it.
    #[test]
    fn blocked_by_heap_write_before_use() {
        let write = WirInstr::StructSet {
            type_id: tid(),
            field_name: "f".to_string(),
            expr: Box::new(lget("obj")),
            value: Box::new(WirInstr::I32Const(1)),
        };
        // `(struct.set …, *b)` modelled as a two-arg call so both are siblings.
        let instr = call(vec![write, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A statement with no box use and no side effect is Pure — the scan may
    /// safely skip it (it never does in practice, but the classification must
    /// not report a spurious barrier).
    #[test]
    fn pure_when_no_use() {
        let instr = WirInstr::I32Add(Box::new(lget("x")), Box::new(WirInstr::I32Const(1)));
        assert!(matches!(walk(&instr), BoxUseWalk::Pure));
    }

    /// A heap read (non-target `StructGet`) before the use is fine — reads
    /// never change what another read observes, and a trap aborts before the
    /// pure moved read matters. Found.
    #[test]
    fn found_use_after_heap_read() {
        let other_read = WirInstr::StructGet {
            type_id: tid(),
            field_name: "other".to_string(),
            expr: Box::new(lget("obj")),
            result_ty: WirType::I32,
        };
        let instr = call(vec![other_read, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// End-to-end substitution: `substitute_box_use` replaces the single use
    /// with the initializer and reports success.
    #[test]
    fn substitute_replaces_single_use() {
        let inner = WirInstr::I32Const(42);
        let mut instr = call(vec![lget("other"), use_box()]);
        assert!(substitute_box_use(&mut instr, NAME, FIELD, &inner));
        let WirInstr::Call { args, .. } = &instr else {
            panic!("expected call");
        };
        assert!(matches!(args[1], WirInstr::I32Const(42)));
    }
}
