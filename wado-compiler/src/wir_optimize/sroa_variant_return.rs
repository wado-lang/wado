//! Variant return SROA (Scalar Replacement of Aggregates) for WIR.
//!
//! Rewrites internal functions that return a variant lowered as
//! `(i32 disc, payload_0, payload_1, ...)` into Wasm multi-value returns,
//! eliminating GC struct allocation at function boundaries for the
//! variant-case ref.
//!
//! Tuple and user-struct return ABIs were lifted to a TIR-level
//! classification (`optimize::multi_value_return`); this pass handles only
//! the variant case, whose layout (shared-vs-per-case payload offsets) is
//! WIR-specific.
//!
//! ## Two entry points, one layout engine
//!
//! [`compute_variant_layout`] is the single source of truth for how a variant
//! packs into a result vector. It drives both:
//!
//! - [`sroa_variant_returns`] — widens a function whose sole result is a boxed
//!   `ref Variant` into that layout.
//! - [`flatten_variant_slots`] — once a multi-value function has a `ref W`
//!   *result slot* whose `W` is itself a small variant (the `Ok(ref Option<T>)`
//!   an outer widening leaves boxed), splits that slot into `W`'s own layout,
//!   run to a fix-point so nested-in-nested slots peel one level per round.
//!   This generalizes the layout analysis recursively to any eligible
//!   `SubtypeHierarchy` variant — `Result`, a multi-case variant, an
//!   `Option<(scalar, scalar)>` — not just `Option<scalar>`. Return
//!   decomposition ([`pad_variant_fields`]) and call-site replacement
//!   ([`build_variant_replacement`]) are shared with the widening path.
//!
//! ## Tail-call propagation
//!
//! Eligibility is computed in a fix-point loop so that `return another_call(...)`
//! at the source level (which lowers to a `Return { Some(Call(g)) }` after
//! lowering) can also qualify when `g` is itself a candidate returning the same
//! variant type. Without this, helpers like `deserialize_i64` (whose body ends
//! in `return parse_i64_direct()` / `return parse_i64_from(...)`) stay boxed
//! because their returns aren't direct `StructNew` shapes — they pass through
//! a sub-call's `Result<T, E>`. The seed round still requires all returns to be
//! `StructNew` of variant case types; subsequent rounds additionally accept
//! `Return { Some(Call(c)) }` where `c` is already in the candidate set with a
//! matching variant return type.

use crate::compiler_trace;
use crate::hashmap::IndexSet;
use crate::wir::{
    WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId, WirVariantRepr,
    WirVariantType,
};

use super::util::{collect_pinned_func_ids, is_root_observable};

/// Result-vector cap for the shared (homogeneous) layout:
/// 1 discriminant + 3 shared payload slots.
const MAX_SHARED_RESULT_FIELDS: usize = 4;
/// Result-vector cap for the per-case layout:
/// 1 discriminant + up to 7 per-case payload slots.
const MAX_PER_CASE_RESULT_FIELDS: usize = 8;

/// Variant-return SROA (Scalar Replacement of Aggregates).
///
/// Rewrites internal functions that return a variant into functions that
/// return `[i32 disc, payload_0, payload_1, ...]` directly (Wasm
/// multi-value return). At call sites, the struct allocation + field
/// extraction is replaced with `MultiValueLocalBind` of the discriminant
/// + payload fields.
///
/// A function is eligible when:
/// - It is not exported, not in an element table, and not referenced by
///   `RefFunc`.
/// - Its single return type is a non-nullable `Ref` to a
///   `WirTypeDef::Variant` whose total result-vector arity (1 disc + max
///   payload count) is 2-4.
/// - Every `Return` in the body wraps a `StructNew` of one of the
///   variant's case types.
/// - Every call site stores the result into a temp and reads only via
///   `StructGet`.
pub(super) fn sroa_variant_returns(module: &mut WirPackage) {
    // Collect pinned func_ids (exported, in element tables, or RefFunc'd).
    let pinned = collect_pinned_func_ids(module);

    // Phase 0: elide return-only temps. NIR's `field_scalarize` (HFS) emits
    // `__hfs_call_N` locals that ferry a match arm's value into the
    // surrounding function's `Return`, producing pairs of
    // `LocalSet(__hfs_call_N, StructNew(...)); Return(LocalGet(__hfs_call_N))`.
    // The intermediate `LocalGet` blocks `value_expr_is_variant_struct_new`
    // (which looks for `StructNew`/`Call`/`Unreachable` at the Return leaf),
    // so chains like `deserialize_i64 → parse_i64_direct` can't propagate
    // through `parse_i64_direct`. Collapsing every paired
    // `LocalSet → Return(LocalGet)` into a direct `Return(value)` is sound
    // when the temp's *only* uses match that shape — no other reads and no
    // unpaired writes — which is exactly the HFS-synthesised pattern.
    //
    // Skip pinned functions: SROA never touches them anyway, so any subtle
    // bug in the elision peephole shouldn't affect exported entry points.
    elide_return_only_temps(module, &pinned);

    // Phase 1: identify candidate functions.
    let candidates = find_sroa_candidates(module, &pinned);
    compiler_trace!("sroa_variant_return", "candidates = {}", candidates.len());
    if candidates.is_empty() {
        return;
    }

    // Phase 2: validate call sites across all function bodies.
    let confirmed = validate_call_sites(module, &candidates);
    compiler_trace!("sroa_variant_return", "confirmed = {}", confirmed.len());
    if confirmed.is_empty() {
        return;
    }

    // Phase 3: rewrite confirmed functions and their call sites.
    apply_sroa(module, &confirmed);
}

/// Information about a variant-return SROA candidate function.
#[derive(Clone)]
struct SroaCandidate {
    /// Index into `module.functions`.
    func_array_idx: usize,
    /// The WIR type index of the variant being returned.
    struct_type_idx: u32,
    /// WIR type indices that are valid `StructNew` targets at the leaf of a
    /// `Return` — every case struct of the variant plus the base variant
    /// type (for unit cases). Cached so the validation phase can re-run
    /// `all_returns_are_variant_struct_new` when an invalidation removes a
    /// tail-call target and the surrounding caller's return shape must be
    /// rechecked (cascade invalidation).
    valid_case_type_indices: IndexSet<u32>,
    /// The field types of the new multi-value result types:
    /// `[i32 (discriminant), payload_type_0, payload_type_1, ...]`.
    field_types: Vec<WirType>,
    /// Number of multi-value result fields.
    field_count: usize,
    /// Field names for the multi-value results:
    /// `["discriminant", "payload_0", "payload_1", ...]`.
    field_names: Vec<String>,
    /// Variant-specific layout info.
    variant_info: VariantSroaInfo,
}

/// Additional info needed for variant SROA.
#[derive(Clone)]
struct VariantSroaInfo {
    /// WIR type indices of the case struct types (index = case discriminant).
    case_type_indices: Vec<Option<u32>>,
    /// Total payload slot count in the multi-value result.
    max_payload_count: usize,
    /// Per-case payload slot offsets in the multi-value result.
    /// `case_slot_offsets[case_discriminant]` is the starting index (0-based)
    /// within the payload portion (after the discriminant) for that case's payloads.
    /// `None` means this case uses shared (homogeneous) layout.
    case_slot_offsets: Option<Vec<usize>>,
}

/// Replacement info for a variant SROA'd temp local at call sites.
struct VariantReplacement {
    /// Local name holding the discriminant value.
    disc_local: String,
    /// `case_wir_type_idx` → discriminant value (i32).
    case_disc_values: crate::hashmap::IndexMap<u32, i32>,
    /// `(case_wir_type_idx, field_name_in_case_struct)` → sroa local name.
    field_to_local: crate::hashmap::IndexMap<(u32, String), String>,
    /// SROA locals that hold ref types (need `ref.as_non_null` when read).
    ref_locals: crate::hashmap::IndexSet<String>,
}

/// Returns true if a `WirType` is a valid Wasm value type for multi-value returns.
///
/// Whitelist: every type here has a Wasm value-type representation and a
/// default padding value in [`default_value_for_type`]. Concrete GC refs are
/// eligible (Wasm multi-value returns support any value type). Abstract heap
/// refs are excluded as they lack the precise type information needed for
/// `StructGet` replacement; `Unit` has no Wasm representation.
fn is_eligible_field_type(ty: &WirType) -> bool {
    match ty {
        WirType::I8
        | WirType::I16
        | WirType::I32
        | WirType::I64
        | WirType::U8
        | WirType::U16
        | WirType::U32
        | WirType::U64
        | WirType::F32
        | WirType::F64
        | WirType::V128
        | WirType::Bool
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. }
        | WirType::Ref { .. } => true,
        WirType::AbstractRef { .. } | WirType::Unit => false,
    }
}

/// Per-function statistics for return-only temp elision.
#[derive(Default)]
struct ReturnTempStats {
    /// Total `LocalSet` (and `LocalTee`) writes to this name, anywhere.
    total_writes: usize,
    /// Subset of writes that sit immediately before `Return(LocalGet(name))`
    /// in the same statement list.
    paired_writes: usize,
    /// Set to true on any use of `name` that disqualifies the temp:
    ///   - a `LocalGet(name)` *not* inside a `Return(LocalGet(name))` statement;
    ///   - a `LocalSet` / `LocalTee` to `name` that appears as a sub-expression
    ///     (not a top-level statement in some block / seq / if / loop body);
    ///   - a `LocalTee(name, _)` anywhere (consumes-and-leaves-on-stack — fine
    ///     in WIR but not the paired shape we're matching).
    has_other_use: bool,
}

/// Phase 0: collapse `LocalSet(temp, X) ; [intervening stmts] ;
/// Return(LocalGet(temp))` triples to `Return(X)`. NIR `field_scalarize`
/// (HFS) introduces `__hfs_call_N` locals to capture a match arm's value
/// before convergence sync, even when the captured value flows straight
/// into the surrounding function's `Return`. The pattern in WIR is
///
/// ```text
/// __hfs_call_N = struct.new Result::Err { ... };
/// self.pos = __hfs_pos_X;                       // HFS write-back
/// return __hfs_call_N;
/// ```
///
/// The intermediate `LocalGet(name)` would otherwise hide a `StructNew`
/// leaf from the return-shape checker, blocking the variant-return SROA
/// candidate analysis. Eliding the temp leaves the equivalent
/// `Return(StructNew)` shape that the analysis already understands.
///
/// Soundness: rewriting one such triple moves `X` past the intervening
/// stmts to the `Return` site, so we require:
///   1. Every `LocalGet(name)` is the paired `Return(LocalGet(name))`
///      value, and every write to `name` is paired with such a `Return`
///      after zero or more intervening stmts that don't reference `name`.
///   2. `X` reads no heap / struct / array / memory / global state and
///      contains no `Call*` — i.e. its value is fixed by the locals it
///      reads at the moment it would have been evaluated. Any intervening
///      `StructSet` / memory store therefore can't change `X`'s value.
///   3. The intervening stmts' local-state effects are disjoint from
///      `X`'s local-state effects: nothing intervening writes a local
///      `X` reads, and nothing intervening reads a local `X` writes
///      (e.g. an internal `offset_N = …; struct.new …`).
///
/// HFS-synthesised values consist of locals + literals + pure arithmetic
/// plus a few internal `LocalSet` temps; their reads / writes are disjoint
/// from the surrounding `self.pos = __hfs_pos_X` HFS write-back, so they
/// all pass these checks. `Call`-shaped values fail check (2) — a sub-call
/// reordered past a `StructSet` could observe different state.
///
/// Values that can *trap* (division, casts, truncations, …) still qualify:
/// a trap depends only on the locally-read operands, not on heap state.
/// [`find_paired_return`] separately forbids relocating a trap-capable
/// value past intervening statements, where the reorder would move the
/// trap across observable effects or control-flow exits.
fn is_root_heap_read(expr: &WirInstr) -> bool {
    matches!(
        expr,
        WirInstr::StructGet { .. }
            | WirInstr::ArrayGet { .. }
            | WirInstr::ArrayGetS { .. }
            | WirInstr::ArrayGetU { .. }
            | WirInstr::ArrayLen(_)
            | WirInstr::I32Load { .. }
            | WirInstr::I32Load8U { .. }
            | WirInstr::I32Load8S { .. }
            | WirInstr::I32Load16U { .. }
            | WirInstr::I32Load16S { .. }
            | WirInstr::I64Load { .. }
            | WirInstr::V128Load { .. }
            | WirInstr::GlobalGet { .. }
            | WirInstr::TableGet { .. }
            // `memory.size` observes the post-growth size of linear memory;
            // moving it past an intervening `memory.grow` would observe a
            // different value.
            | WirInstr::MemorySize
    )
}

fn reads_only_local_state(expr: &WirInstr) -> bool {
    // Internal `LocalSet` / `LocalTee` temps are fine: `collect_local_io`
    // tracks their reads / writes for the disjointness check against
    // intervening statements. Everything else observable at the root
    // (calls, stores, control-flow exits, traps-as-statements) or reading
    // non-local state pins the value to its original program point.
    if !matches!(expr, WirInstr::LocalSet { .. } | WirInstr::LocalTee { .. })
        && (is_root_observable(expr) || is_root_heap_read(expr))
    {
        return false;
    }
    let mut ok = true;
    expr.for_each_child(&mut |child| {
        if ok && !reads_only_local_state(child) {
            ok = false;
        }
    });
    ok
}

/// Trap capability of a value that already passed
/// [`reads_only_local_state`] (so heap reads, calls, stores, and
/// control-flow exits are absent). A local refinement of
/// [`util::may_trap`](super::util::may_trap): `array.new_data` is treated
/// as non-trapping because WIR only ever carries literal-lowered segments
/// with a zero offset and the segment's exact length
/// (`wir_build::primitive_ops::translate_packed_array`,
/// `wir_optimize::array::promote_constant_arrays_to_data`) — `may_trap`
/// stays conservative for its `Drop`-preservation duty, where a false
/// positive is harmless, but here it would block eliding every
/// `Err("<literal beyond the inline threshold>")` return past HFS
/// write-backs. Candidate for consolidation into `util`.
fn relocated_value_may_trap(expr: &WirInstr) -> bool {
    match expr {
        WirInstr::ArrayNewData { offset, len, .. } => {
            relocated_value_may_trap(offset) || relocated_value_may_trap(len)
        }
        // Operand-aware refinements, mirroring `may_trap`.
        WirInstr::RefAsNonNull(inner) => {
            relocated_value_may_trap(inner) || !inner.is_nonnull_result()
        }
        WirInstr::RefCast { type_id, expr, .. } => match expr.as_ref() {
            WirInstr::StructNew {
                type_id: src_type, ..
            } if src_type == type_id => relocated_value_may_trap(expr),
            _ => true,
        },
        // Integer divide / remainder trap on a zero divisor (and signed
        // MIN / -1); non-saturating float-to-int truncation traps on
        // out-of-range values.
        WirInstr::I32DivS(_, _)
        | WirInstr::I32DivU(_, _)
        | WirInstr::I32RemS(_, _)
        | WirInstr::I32RemU(_, _)
        | WirInstr::I64DivS(_, _)
        | WirInstr::I64DivU(_, _)
        | WirInstr::I64RemS(_, _)
        | WirInstr::I64RemU(_, _)
        | WirInstr::I32TruncF32S(_)
        | WirInstr::I32TruncF32U(_)
        | WirInstr::I32TruncF64S(_)
        | WirInstr::I32TruncF64U(_)
        | WirInstr::I64TruncF32S(_)
        | WirInstr::I64TruncF32U(_)
        | WirInstr::I64TruncF64S(_)
        | WirInstr::I64TruncF64U(_) => true,
        other => {
            let mut trap = false;
            other.for_each_child(&mut |child| {
                if !trap && relocated_value_may_trap(child) {
                    trap = true;
                }
            });
            trap
        }
    }
}

/// Collect every local name read (`LocalGet`) or written (`LocalSet` /
/// `LocalTee`) anywhere in `expr`'s subtree. Used to verify that
/// relocating `X` past intervening stmts preserves observable behaviour.
fn collect_local_io(expr: &WirInstr, reads: &mut IndexSet<String>, writes: &mut IndexSet<String>) {
    match expr {
        WirInstr::LocalGet { name, .. } => {
            reads.insert(name.clone());
        }
        WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
            writes.insert(name.clone());
            collect_local_io(value, reads, writes);
        }
        _ => {
            expr.for_each_child(&mut |child| collect_local_io(child, reads, writes));
        }
    }
}
fn elide_return_only_temps(module: &mut WirPackage, pinned: &IndexSet<u32>) {
    let defined_func_base = module.defined_func_base;
    for (i, func) in module.functions.iter_mut().enumerate() {
        let func_id_index = defined_func_base + u32::try_from(i).unwrap();
        // Pinned functions (exports, RefFunc'd, element-table entries) are
        // SROA-ineligible anyway; skipping them limits the blast radius of
        // any bug in this peephole to functions that the rest of the pass
        // would have rewritten too.
        if pinned.contains(&func_id_index) {
            continue;
        }
        if let Some(body) = &mut func.body {
            elide_return_only_temps_in_body(body);
        }
    }
}

fn elide_return_only_temps_in_body(body: &mut [WirInstr]) {
    let mut stats: crate::hashmap::IndexMap<String, ReturnTempStats> =
        crate::hashmap::IndexMap::default();
    scan_return_temp_stats(body, &mut stats);

    let valid: IndexSet<String> = stats
        .iter()
        .filter(|(_, s)| {
            !s.has_other_use && s.paired_writes == s.total_writes && s.total_writes > 0
        })
        .map(|(name, _)| name.clone())
        .collect();
    if valid.is_empty() {
        return;
    }
    rewrite_return_temp_pairs(body, &valid);
}

/// Walk every statement list reachable from `body`, recording pair-vs-other
/// use counts for every local that participates in a `LocalSet` /
/// `LocalGet`. Statement-list contexts are: the top-level function body,
/// `Block` / `Loop` bodies, `If` `then_body` / `else_body`, and explicit
/// `Seq` instructions. A `LocalSet(name, X)` followed (after zero or
/// more intervening stmts that don't reference `name` and don't conflict
/// with `X`'s reads / writes) by `Return(LocalGet(name))` at the same
/// level counts as one paired write and one paired read; the `LocalGet`
/// inside the `Return` is *not* counted as a separate use. Any other
/// shape of write or read tips `has_other_use = true` so the temp is
/// rejected.
fn scan_return_temp_stats(
    instrs: &[WirInstr],
    stats: &mut crate::hashmap::IndexMap<String, ReturnTempStats>,
) {
    let mut i = 0;
    while i < instrs.len() {
        if let WirInstr::LocalSet { name, value } = &instrs[i]
            && reads_only_local_state(value)
            && let Some(return_idx) = find_paired_return(instrs, i, name, value)
        {
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.paired_writes += 1;
            // The pair's `value` may still contain other locals' uses; the
            // LocalGet of `name` inside the Return is *consumed* by the pair
            // and not counted here. Any other use of `name` inside `value`
            // (unusual but possible — e.g. `__hfs_call = f(__hfs_call)`) is a
            // disqualifier.
            scan_return_temp_uses_in_expr(value, Some(name.as_str()), stats);
            // The intervening stmts (i+1 .. return_idx) were already
            // disjointness-checked by `find_paired_return`. Still walk them
            // for the surrounding scan: a LocalGet of *another* temp in those
            // stmts must still count as an "other read" for that other temp.
            for j in (i + 1)..return_idx {
                scan_return_temp_uses_in_instr(&instrs[j], stats);
            }
            i = return_idx + 1;
            continue;
        }
        scan_return_temp_uses_in_instr(&instrs[i], stats);
        i += 1;
    }
}

/// Maximum number of intervening statements between the `LocalSet(temp, X)`
/// and its paired `Return(LocalGet(temp))`. HFS write-back sequences sit
/// here; nested struct deserializers can emit several restore-and-store
/// statements before the return. 32 covers the patterns observed in
/// `core:json` and `core:serde` deserializers without being open-ended (a
/// truly large gap is unlikely to be safe to relocate past anyway).
const RETURN_TEMP_INTERVENING_BUDGET: usize = 32;

/// Return the index of a `Return(LocalGet(name))` reachable from
/// `start_idx + 1` through intervening stmts that
///   - don't reference `name`,
///   - don't write any local that `value` reads, and
///   - don't read any local that `value` writes.
///
/// A trap-capable `value` pairs only with the *adjacent* `Return`
/// (`start_idx + 1`): relocating a trap past intervening statements would
/// reorder it across their effects — in particular past a conditional
/// `return` / `br` exit, on whose path the trap would then be lost
/// entirely (`t = a / b; if c { return OTHER; } return t;`).
///
/// Returns `None` when no such return exists within
/// [`RETURN_TEMP_INTERVENING_BUDGET`] stmts.
fn find_paired_return(
    instrs: &[WirInstr],
    start_idx: usize,
    name: &str,
    value: &WirInstr,
) -> Option<usize> {
    let mut x_reads: IndexSet<String> = IndexSet::default();
    let mut x_writes: IndexSet<String> = IndexSet::default();
    collect_local_io(value, &mut x_reads, &mut x_writes);
    let value_may_trap = relocated_value_may_trap(value);

    let end = (start_idx + 1 + RETURN_TEMP_INTERVENING_BUDGET).min(instrs.len());
    for j in (start_idx + 1)..end {
        if let WirInstr::Return { value: Some(rv) } = &instrs[j]
            && let WirInstr::LocalGet { name: rn, .. } = rv.as_ref()
            && rn == name
        {
            if value_may_trap && j > start_idx + 1 {
                return None;
            }
            return Some(j);
        }
        // Intervening stmt — verify it doesn't reference `name` and is
        // disjoint from `X`'s local I/O.
        let mut intervening_reads: IndexSet<String> = IndexSet::default();
        let mut intervening_writes: IndexSet<String> = IndexSet::default();
        collect_local_io(&instrs[j], &mut intervening_reads, &mut intervening_writes);
        if intervening_reads.contains(name) || intervening_writes.contains(name) {
            return None;
        }
        if x_reads.iter().any(|r| intervening_writes.contains(r)) {
            return None;
        }
        if x_writes.iter().any(|w| intervening_reads.contains(w)) {
            return None;
        }
    }
    None
}

/// Record uses of every local appearing inside `instr`. When
/// `skip_name == Some(n)`, a top-level `LocalGet(n)` is *not* recorded —
/// used by the pair detector so the paired `LocalGet` doesn't double-count
/// as an "other read". Recurses into child instructions; statement-list
/// children (Block / Loop / If / Seq bodies) re-enter the pair detector
/// via [`scan_return_temp_stats`].
fn scan_return_temp_uses_in_instr(
    instr: &WirInstr,
    stats: &mut crate::hashmap::IndexMap<String, ReturnTempStats>,
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            scan_return_temp_stats(body, stats);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            scan_return_temp_uses_in_expr(condition, None, stats);
            scan_return_temp_stats(then_body, stats);
            if let Some(eb) = else_body {
                scan_return_temp_stats(eb, stats);
            }
        }
        WirInstr::Seq(body) => {
            scan_return_temp_stats(body, stats);
        }
        WirInstr::LocalSet { name, value } => {
            // Unpaired LocalSet (the pair branch would have handled it).
            // Count the write but disqualify the temp.
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.has_other_use = true;
            scan_return_temp_uses_in_expr(value, None, stats);
        }
        WirInstr::LocalTee { name, value } => {
            // LocalTee both writes and leaves the value on the stack, so the
            // temp is observably read at the same time. Always disqualifies.
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.has_other_use = true;
            scan_return_temp_uses_in_expr(value, None, stats);
        }
        other => scan_return_temp_uses_in_expr(other, None, stats),
    }
}

fn scan_return_temp_uses_in_expr(
    expr: &WirInstr,
    skip_name: Option<&str>,
    stats: &mut crate::hashmap::IndexMap<String, ReturnTempStats>,
) {
    if let WirInstr::LocalGet { name, .. } = expr {
        if skip_name != Some(name.as_str()) {
            // Any LocalGet outside a paired Return value is an "other read".
            stats.entry(name.clone()).or_default().has_other_use = true;
        }
        return;
    }
    if matches!(
        expr,
        WirInstr::Block { .. } | WirInstr::Loop { .. } | WirInstr::If { .. } | WirInstr::Seq(_)
    ) {
        scan_return_temp_uses_in_instr(expr, stats);
        return;
    }
    if let WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } = expr {
        // LocalSet / LocalTee appearing as a sub-expression (not a top-level
        // stmt) is always a disqualifier — it's not the paired shape.
        let entry = stats.entry(name.clone()).or_default();
        entry.total_writes += 1;
        entry.has_other_use = true;
        scan_return_temp_uses_in_expr(value, None, stats);
        return;
    }
    if let WirInstr::Return { value: Some(rv) } = expr {
        // Bare `Return(LocalGet(name))` in expression position (shouldn't
        // arise structurally — Return is a statement — but defend anyway).
        if let WirInstr::LocalGet { name, .. } = rv.as_ref() {
            stats.entry(name.clone()).or_default().has_other_use = true;
            return;
        }
        scan_return_temp_uses_in_expr(rv, None, stats);
        return;
    }
    expr.for_each_child(&mut |child| scan_return_temp_uses_in_expr(child, None, stats));
}

/// Rewrite every paired `LocalSet(name, X); [intervening]; Return(LocalGet(name))`
/// where `name` is in `valid` into `Nop; [intervening]; Return(X)`. The
/// original `LocalSet`'s value moves into the `Return`; the `LocalSet`
/// slot becomes a `Nop` so downstream cleanup passes can drop it.
fn rewrite_return_temp_pairs(instrs: &mut [WirInstr], valid: &IndexSet<String>) {
    let mut i = 0;
    while i < instrs.len() {
        // Match the relaxed pair shape: same predicate that
        // `scan_return_temp_stats` accepted as paired.
        if let WirInstr::LocalSet { name, value } = &instrs[i]
            && valid.contains(name.as_str())
            && reads_only_local_state(value)
        {
            let name_owned = name.clone();
            if let Some(return_idx) = find_paired_return(instrs, i, &name_owned, value) {
                let WirInstr::LocalSet { value, .. } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
                else {
                    unreachable!()
                };
                instrs[return_idx] = WirInstr::Return { value: Some(value) };
                // Recurse into the intervening stmts (their nested bodies
                // may still contain other paired triples in nested blocks).
                for j in (i + 1)..return_idx {
                    rewrite_return_temp_pairs_in_instr(&mut instrs[j], valid);
                }
                i = return_idx + 1;
                continue;
            }
        }
        rewrite_return_temp_pairs_in_instr(&mut instrs[i], valid);
        i += 1;
    }
}

fn rewrite_return_temp_pairs_in_instr(instr: &mut WirInstr, valid: &IndexSet<String>) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_return_temp_pairs(body, valid);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            rewrite_return_temp_pairs(then_body, valid);
            if let Some(eb) = else_body {
                rewrite_return_temp_pairs(eb, valid);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_return_temp_pairs(body, valid);
        }
        _ => {}
    }
}

/// Per-function info computed up-front so the fix-point loop over candidate
/// discovery can re-check return shapes without redoing the layout analysis.
struct PotentialCandidate {
    func_id_index: u32,
    variant_type_idx: u32,
    /// WIR type indices that are valid `StructNew` targets at the leaf of a
    /// `Return` — every case struct of the variant plus the base variant
    /// type (for unit cases).
    valid_case_type_indices: IndexSet<u32>,
    candidate: SroaCandidate,
}

/// Phase 1: find functions eligible for SROA.
///
/// Two-stage discovery:
/// 1. Compute layout info (`variant_type_idx`, `valid_case_type_indices`,
///    `SroaCandidate`) for every function whose return type is an in-range
///    variant. This stage does not look at the function body's return shapes.
/// 2. Fix-point: accept a function when every `Return` in its body is either
///    a `StructNew` of a variant case type, an `Unreachable`, or a
///    `Return { Some(Call(c)) }` where `c` is also accepted *and* shares
///    the same variant return type. The fix-point is optimistic
///    (assume-then-refute): every layout-eligible function starts accepted
///    and a round removes those whose return shapes don't hold against the
///    current set. Unlike a pessimistic grow-from-leaves seed, this accepts
///    mutually (and self) tail-recursive candidate groups — each member's
///    tail call targets another member, which stays assumed unless refuted.
fn find_sroa_candidates(module: &WirPackage, pinned: &IndexSet<u32>) -> Vec<(u32, SroaCandidate)> {
    // Stage 1: collect potential candidates with layout info.
    let mut potentials: Vec<PotentialCandidate> = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = module.defined_func_base + u32::try_from(i).unwrap();

        if pinned.contains(&func_id_index) {
            continue;
        }
        if func.body.is_none() {
            continue;
        }

        let type_idx = func.type_id.index();
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx as usize) else {
            continue;
        };
        if func_type.results.len() != 1 {
            continue;
        }
        let WirType::Ref {
            type_id: ref ret_type_id,
            ..
        } = func_type.results[0]
        else {
            continue;
        };
        let ret_type_idx = ret_type_id.index();

        if let Some(WirTypeDef::Variant(variant_type)) = module.types.get(ret_type_idx as usize)
            && let Some((candidate, valid_case_type_indices)) =
                analyze_variant_layout(module, i, ret_type_idx, variant_type)
        {
            potentials.push(PotentialCandidate {
                func_id_index,
                variant_type_idx: ret_type_idx,
                valid_case_type_indices,
                candidate,
            });
        }
    }

    if potentials.is_empty() {
        return Vec::new();
    }

    // Stage 2: optimistic fix-point. A function stays accepted while its
    // return shapes are all StructNew/Unreachable, optionally with
    // `Return { Some(Call(c)) }` tail-calls to still-accepted candidates
    // with the same variant type (same variant -> same multi-value sig, so
    // swapping ABI is sound). Refutation only shrinks the set, so the loop
    // terminates; at the fixed point every member's tail calls target
    // members, which is exactly the property `apply_sroa` relies on.
    let mut accepted: IndexSet<u32> = potentials.iter().map(|p| p.func_id_index).collect();
    loop {
        let mut accepted_by_variant: crate::hashmap::IndexMap<u32, IndexSet<u32>> =
            crate::hashmap::IndexMap::default();
        for p in &potentials {
            if accepted.contains(&p.func_id_index) {
                accepted_by_variant
                    .entry(p.variant_type_idx)
                    .or_default()
                    .insert(p.func_id_index);
            }
        }

        let mut refuted: Vec<u32> = Vec::new();
        for p in &potentials {
            if !accepted.contains(&p.func_id_index) {
                continue;
            }
            let body = module.functions[p.candidate.func_array_idx]
                .body
                .as_ref()
                .unwrap();
            let empty: IndexSet<u32> = IndexSet::default();
            let tail_call_set = accepted_by_variant
                .get(&p.variant_type_idx)
                .unwrap_or(&empty);
            if !all_returns_are_variant_struct_new(body, &p.valid_case_type_indices, tail_call_set)
            {
                refuted.push(p.func_id_index);
            }
        }
        if refuted.is_empty() {
            break;
        }
        for id in refuted {
            accepted.swap_remove(&id);
        }
    }

    potentials
        .into_iter()
        .filter(|p| accepted.contains(&p.func_id_index))
        .map(|p| (p.func_id_index, p.candidate))
        .collect()
}

/// The multi-value layout of a variant: `[i32 disc, payload_0, ...]`.
///
/// The single source of truth for how a variant's cases pack into a Wasm
/// result vector, shared by function-return widening
/// ([`analyze_variant_layout`]) and nested result-slot flattening
/// ([`flatten_variant_slots`]).
struct VariantLayout {
    /// Field types of the result vector (`[i32, payload_types...]`).
    field_types: Vec<WirType>,
    /// Field names (`["discriminant", "payload_0" | "caseN_payload_M", ...]`).
    field_names: Vec<String>,
    /// Per-case slot layout.
    variant_info: VariantSroaInfo,
    /// Every case struct type index plus the base variant type — the valid
    /// `StructNew` targets at a `Return` leaf.
    valid_case_type_indices: IndexSet<u32>,
}

/// Compute the multi-value layout of a variant, or `None` when it does not fit
/// the small-tuple model.
///
/// A variant is eligible (layout-wise) if:
/// - All payload types across all cases are eligible value types
/// - The result vector fits [`MAX_SHARED_RESULT_FIELDS`] (shared layout) or
///   [`MAX_PER_CASE_RESULT_FIELDS`] (per-case layout)
/// - Case type indices can be resolved via `variant_case_info`
fn compute_variant_layout(
    module: &WirPackage,
    variant_type_idx: u32,
    variant_type: &WirVariantType,
) -> Option<VariantLayout> {
    // Collect per-case info: case type index and payload count
    let mut case_type_indices: Vec<Option<u32>> = Vec::with_capacity(variant_type.cases.len());
    let mut max_payload_count: usize = 0;

    // Build a mapping of case_wir_type_idx for this variant from variant_case_info
    let mut case_idx_to_type_idx: crate::hashmap::IndexMap<u32, u32> =
        crate::hashmap::IndexMap::default();
    for (&case_wir_idx, &(parent_variant_idx, case_index)) in &module.variant_case_info {
        if parent_variant_idx == variant_type_idx {
            case_idx_to_type_idx.insert(case_index, case_wir_idx);
        }
    }

    for case in &variant_type.cases {
        let payload_count = case.payload.len();
        if payload_count > max_payload_count {
            max_payload_count = payload_count;
        }

        // Check payload types are eligible
        for ty in &case.payload {
            if !is_eligible_field_type(ty) {
                return None;
            }
        }

        if payload_count > 0 {
            // Must have a case type registered
            let &case_type_idx = case_idx_to_type_idx.get(&case.index)?;
            case_type_indices.push(Some(case_type_idx));
        } else {
            case_type_indices.push(None);
        }
    }

    // Total multi-value fields: discriminant + max_payload_count
    let field_count = 1 + max_payload_count;
    if field_count > MAX_SHARED_RESULT_FIELDS {
        return None;
    }

    // Compute the payload types: try shared layout first, fall back to per-case slots.
    let mut homogeneous = true;
    for pos in 0..max_payload_count {
        let mut found: Option<&WirType> = None;
        for case in &variant_type.cases {
            if let Some(ty) = case.payload.get(pos) {
                if let Some(existing) = found {
                    if existing != ty {
                        homogeneous = false;
                        break;
                    }
                } else {
                    found = Some(ty);
                }
            }
        }
        if !homogeneous {
            break;
        }
    }

    let (field_types, field_names, field_count, case_slot_offsets) = if homogeneous {
        // Shared layout: all cases use the same type at each position
        let mut payload_types: Vec<WirType> = Vec::with_capacity(max_payload_count);
        for pos in 0..max_payload_count {
            let mut found: Option<&WirType> = None;
            for case in &variant_type.cases {
                if let Some(ty) = case.payload.get(pos) {
                    found = Some(ty);
                    break;
                }
            }
            payload_types.push(found?.clone());
        }
        let fc = 1 + max_payload_count;
        let mut ft = Vec::with_capacity(fc);
        ft.push(WirType::I32);
        ft.extend(payload_types.into_iter().map(WirType::as_nullable));
        let mut fn_ = Vec::with_capacity(fc);
        fn_.push("discriminant".to_string());
        for pos in 0..max_payload_count {
            fn_.push(format!("payload_{pos}"));
        }
        (ft, fn_, fc, None)
    } else {
        // Per-case layout: each case gets its own payload slots
        // Layout: [disc, case0_payload_0, ..., case1_payload_0, ...]
        let mut ft = vec![WirType::I32];
        let mut fn_ = vec!["discriminant".to_string()];
        let mut offsets = Vec::with_capacity(variant_type.cases.len());
        for (case_idx, case) in variant_type.cases.iter().enumerate() {
            let offset = ft.len() - 1; // offset within payload portion (after disc)
            offsets.push(offset);
            for (pos, ty) in case.payload.iter().enumerate() {
                ft.push(WirType::as_nullable(ty.clone()));
                fn_.push(format!("case{case_idx}_payload_{pos}"));
            }
        }
        let fc = ft.len();
        if fc > MAX_PER_CASE_RESULT_FIELDS {
            return None;
        }
        (ft, fn_, fc, Some(offsets))
    };

    // Recompute max_payload_count for per-case layout: total payload slots (not per-case max)
    let total_payload_slots = field_count - 1;

    // Collect ALL case type indices (including unit cases) for return validation
    let mut all_case_type_indices: IndexSet<u32> = IndexSet::default();
    for &opt in &case_type_indices {
        if let Some(idx) = opt {
            all_case_type_indices.insert(idx);
        }
    }
    // Also include StructNew of the base variant type (for unit cases like None)
    all_case_type_indices.insert(variant_type_idx);

    Some(VariantLayout {
        field_types,
        field_names,
        variant_info: VariantSroaInfo {
            case_type_indices,
            max_payload_count: total_payload_slots,
            case_slot_offsets,
        },
        valid_case_type_indices: all_case_type_indices,
    })
}

/// Compute the SROA layout info for a variant-returning function.
///
/// Wraps [`compute_variant_layout`] into a [`SroaCandidate`]. The caller
/// separately verifies that every `Return` in the body is a leaf shape
/// compatible with this layout (`all_returns_are_variant_struct_new`), so this
/// stage can be re-used across the fix-point's rounds without touching the body.
fn analyze_variant_layout(
    module: &WirPackage,
    func_array_idx: usize,
    variant_type_idx: u32,
    variant_type: &WirVariantType,
) -> Option<(SroaCandidate, IndexSet<u32>)> {
    let layout = compute_variant_layout(module, variant_type_idx, variant_type)?;
    let valid = layout.valid_case_type_indices.clone();
    Some((
        SroaCandidate {
            func_array_idx,
            struct_type_idx: variant_type_idx,
            valid_case_type_indices: valid.clone(),
            field_count: layout.field_types.len(),
            field_types: layout.field_types,
            field_names: layout.field_names,
            variant_info: layout.variant_info,
        },
        valid,
    ))
}

/// Check that every `Return` in the body produces a leaf shape we can rewrite:
/// a `StructNew` of one of the variant's case types, an `Unreachable`, or a
/// `Return { Some(Call(c)) }` where `c` is a `tail_call_candidate` (a function
/// already accepted with the same variant return type).
fn all_returns_are_variant_struct_new(
    instrs: &[WirInstr],
    valid_type_indices: &IndexSet<u32>,
    tail_call_candidates: &IndexSet<u32>,
) -> bool {
    for instr in instrs {
        if !check_return_variant_struct_new(instr, valid_type_indices, tail_call_candidates) {
            return false;
        }
    }
    true
}

fn check_return_variant_struct_new(
    instr: &WirInstr,
    valid_type_indices: &IndexSet<u32>,
    tail_call_candidates: &IndexSet<u32>,
) -> bool {
    match instr {
        WirInstr::Return { value: Some(v) } => {
            // Tail-position only: accept `Return { Some(Call(candidate)) }`
            // here, and recurse through linear `Seq` sequencing. The
            // strict (StructNew-only) variant is used for nested branching
            // value contexts where a Call leaf would leak multi-value
            // results past the merge point.
            //
            // `top_return_value_compatible` validates the value/tail leaves;
            // `embedded_returns_compatible` additionally validates every
            // `return` statement embedded in non-tail positions of `v` (e.g. a
            // `?`-desugared `return Err(…)` nested in a `let x = if …` binding),
            // since `rewrite_variant_returns_to_multi_value` will rewrite those
            // too and must find a shape it can lower.
            top_return_value_compatible(v, valid_type_indices, tail_call_candidates)
                && embedded_returns_compatible(v, valid_type_indices, tail_call_candidates)
        }
        WirInstr::Return { value: None } => true,
        WirInstr::Block { body, result, .. } => {
            let inner_ok =
                all_returns_are_variant_struct_new(body, valid_type_indices, tail_call_candidates);
            if result.is_some() {
                // Typed block: the block's exit values are carried via [val, Br(0)] pairs.
                // These Br-exit values must also be StructNew of valid variant case types,
                // otherwise the function cannot be correctly SROA'd.
                inner_ok && all_br_variant_values_are_struct_new(body, valid_type_indices, 0)
            } else {
                inner_ok
            }
        }
        WirInstr::Loop { body, .. } => {
            all_returns_are_variant_struct_new(body, valid_type_indices, tail_call_candidates)
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            // The condition is an expression that may embed a `Return` (e.g.
            // `if expr? == x { … }`). Validate those returns too, so the apply
            // phase (which now rewrites condition returns) never finds a return
            // shape it cannot lower.
            embedded_returns_compatible(condition, valid_type_indices, tail_call_candidates)
                && all_returns_are_variant_struct_new(
                    then_body,
                    valid_type_indices,
                    tail_call_candidates,
                )
                && else_body.as_ref().is_none_or(|eb| {
                    all_returns_are_variant_struct_new(eb, valid_type_indices, tail_call_candidates)
                })
        }
        WirInstr::Seq(body) => {
            all_returns_are_variant_struct_new(body, valid_type_indices, tail_call_candidates)
        }
        WirInstr::Drop(inner) => {
            check_return_variant_struct_new(inner, valid_type_indices, tail_call_candidates)
        }
        // Other statements can embed a `Return` in a value position the arms
        // above skip — e.g. `LocalSet(t, if … else { return Err(…) })`. The
        // rewriter descends into them (`for_each_boxed_child_mut`), so the
        // validator must too, or it confirms a function with an unrewritable
        // return (`Return(LocalGet(hfs_temp))` left un-elided behind a
        // side-effecting statement) that the rewriter leaves boxed.
        other => embedded_returns_compatible(other, valid_type_indices, tail_call_candidates),
    }
}

/// Validate every `Return` embedded anywhere in an expression subtree — an
/// `if`/`while` condition that contains a `?`-desugared `return Err(…)`, or a
/// non-tail statement of a returned `match`/`if` value (e.g. `return match … {
/// Ok(v) => { let x = helper(v)?; … } }`). Each must be a shape the apply phase
/// can lower to the multi-value return; if any is not, the function is rejected
/// as an SROA candidate so its signature is never rewritten out from under a
/// boxed return.
fn embedded_returns_compatible(
    instr: &WirInstr,
    valid_type_indices: &IndexSet<u32>,
    tail_call_candidates: &IndexSet<u32>,
) -> bool {
    if let WirInstr::Return { value: Some(v) } = instr
        && !top_return_value_compatible(v, valid_type_indices, tail_call_candidates)
    {
        return false;
    }
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !embedded_returns_compatible(child, valid_type_indices, tail_call_candidates) {
            ok = false;
        }
    });
    ok
}

/// Check if an instruction contains `Unreachable` (indicating dead code).
fn contains_unreachable(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::Unreachable => true,
        WirInstr::Seq(items) => items.iter().any(contains_unreachable),
        _ => false,
    }
}

/// Strict (branching-safe) variant of the value-position checker. Accepts
/// `StructNew(case)` and `Unreachable` leaves, recurses through `Seq`/`If`/
/// `Block`. Used inside branching value contexts (typed `If`/`Block` arms,
/// block `Br` exits) where any leaf that pushes values must match the
/// surrounding result type's arity exactly — so tail-call `Call`s (which now
/// produce N values from the rewritten multi-value callee) are not allowed
/// here. The `top_return_value_compatible` wrapper relaxes this for the
/// single tail-position case `Return { Some(_) }`.
///
/// Accepting `Unreachable` is sound and was already implicit for `Br` exits
/// via `contains_unreachable`; mirroring it here lets functions with a
/// trailing `else { unreachable; }` exhaustiveness fallback (very common
/// from match-on-Result) become candidates.
fn value_expr_is_variant_struct_new(expr: &WirInstr, valid_type_indices: &IndexSet<u32>) -> bool {
    match expr {
        WirInstr::StructNew { type_id, .. } => valid_type_indices.contains(&type_id.index()),
        WirInstr::Unreachable => true,
        WirInstr::Seq(items) => items
            .last()
            .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices)),
        WirInstr::If {
            then_body,
            else_body,
            result: Some(_),
            ..
        } => {
            let then_ok = then_body
                .last()
                .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices));
            let else_ok = else_body.as_ref().is_some_and(|eb| {
                eb.last()
                    .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices))
            });
            then_ok && else_ok
        }
        WirInstr::Block {
            body,
            result: Some(_),
            ..
        } => {
            // A typed block produces a value via two paths: every
            // `[val, Br(0)]` exit pair, and (if the body doesn't end in a
            // divergent instruction) the fallthrough value of the last
            // item. The Br-exit values are checked by
            // `all_br_variant_values_are_struct_new`; the fallthrough also
            // needs validation, otherwise a body like
            // `[LocalSet(x, Call), StructGet(RefCast(...))]` would be
            // accepted vacuously (no Br exits, fallthrough not checked) and
            // the rewriter would clear the block's result type with no
            // matching StructNew leaf, producing invalid Wasm.
            all_br_variant_values_are_struct_new(body, valid_type_indices, 0)
                && block_fallthrough_is_variant_compatible(body, valid_type_indices)
        }
        _ => false,
    }
}

/// True when `body`'s fallthrough value path is acceptable for variant SROA:
/// either the body diverges (last is `Return` / `Unreachable` / `Br*`) so no
/// fallthrough value is produced, or the last instruction is itself one of
/// the accepted leaf shapes recognised by `value_expr_is_variant_struct_new`.
fn block_fallthrough_is_variant_compatible(
    body: &[WirInstr],
    valid_type_indices: &IndexSet<u32>,
) -> bool {
    let Some(last) = body.last() else {
        return true;
    };
    match last {
        WirInstr::Return { .. }
        | WirInstr::Unreachable
        | WirInstr::Br { .. }
        | WirInstr::BrIf { .. }
        | WirInstr::BrTable { .. } => true,
        _ => value_expr_is_variant_struct_new(last, valid_type_indices),
    }
}

/// Tail-position relaxation of `value_expr_is_variant_struct_new`. Accepts
/// the strict shape *plus* a literal `Call(candidate)` at the very top of
/// the `Return { Some(_) }` value. Anything more deeply nested (through
/// `Seq`, `If`, or `Block`) falls back to the strict checker so the
/// rewrite phase's "clear merge-point result type and wrap `StructNew`
/// leaves in Return" transform stays well-typed: a `Call(candidate)` leaf
/// inside a typed merge point would push N multi-values with nowhere to
/// go after the merge-point result type is cleared.
///
/// **Asymmetry with `unwrap_to_candidate_call`.** Validation's
/// `unwrap_to_candidate_call` *does* look through `Seq` / `Block`
/// wrappers (necessary so the idiomatic `LocalSet(name, Seq([…, Call]))`
/// lowering is recognised as a call site). This discovery-side checker
/// is strictly tighter — only the bare top-level `Call` shape produces
/// a candidate via tail-call propagation. The asymmetry is intentional:
/// the validator's prefix scan
/// (`validate_wrapper_prefixes` → `validate_call_sites_in_body`)
/// reuses the standard call-site logic on the wrapper's prefix, so an
/// inner `LocalSet(temp, Call(g))` with valid variant-access uses is
/// accepted instead of being over-invalidated.
fn top_return_value_compatible(
    v: &WirInstr,
    valid_type_indices: &IndexSet<u32>,
    tail_call_candidates: &IndexSet<u32>,
) -> bool {
    match v {
        WirInstr::Call { func_id, .. } => tail_call_candidates.contains(&func_id.index()),
        other => value_expr_is_variant_struct_new(other, valid_type_indices),
    }
}

/// Variant version of `all_br_values_are_struct_new`.
fn all_br_variant_values_are_struct_new(
    instrs: &[WirInstr],
    valid_type_indices: &IndexSet<u32>,
    target_depth: u32,
) -> bool {
    let mut i = 0;
    while i < instrs.len() {
        if i + 1 < instrs.len()
            && matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth)
        {
            let is_valid = contains_unreachable(&instrs[i])
                || matches!(&instrs[i], WirInstr::StructNew { type_id, .. } if valid_type_indices.contains(&type_id.index()));
            if !is_valid {
                return false;
            }
            i += 2;
        } else if let WirInstr::Seq(seq) = &instrs[i]
            && seq.last().is_some_and(
                |last| matches!(last, WirInstr::Br { depth } if *depth == target_depth),
            )
        {
            // Seq([..., val, Br(depth)]): the Br is wrapped in a Seq (LabeledBlock exit pattern).
            // The instruction before the Br within the Seq is the exit value.
            let exit_val = seq.len().checked_sub(2).and_then(|j| seq.get(j));
            let is_valid = exit_val.is_some_and(|v| {
                contains_unreachable(v)
                    || matches!(v, WirInstr::StructNew { type_id, .. } if valid_type_indices.contains(&type_id.index()))
            });
            if !is_valid {
                return false;
            }
            i += 1;
        } else {
            // Recurse into nested control frames. `Block`/`Loop`/`If` add a depth
            // level; a plain `Seq` does not. `Loop` and `Seq` both matter: an
            // inlined helper's tail `block { loop { … break outer: <val> … } }`
            // carries its exit value through a loop- and `Seq`-nested break
            // (`Seq([Seq([Call, Br(d)])])`). Missing either let a
            // `break outer: Call(boxed_fn)` exit escape the StructNew check while
            // the rewriter left its boxed ref under the multi-value signature.
            match &instrs[i] {
                WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                    if !all_br_variant_values_are_struct_new(
                        body,
                        valid_type_indices,
                        target_depth + 1,
                    ) {
                        return false;
                    }
                }
                WirInstr::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    if !all_br_variant_values_are_struct_new(
                        then_body,
                        valid_type_indices,
                        target_depth + 1,
                    ) {
                        return false;
                    }
                    if let Some(eb) = else_body
                        && !all_br_variant_values_are_struct_new(
                            eb,
                            valid_type_indices,
                            target_depth + 1,
                        )
                    {
                        return false;
                    }
                }
                // `Seq` is not a Wasm control frame — recurse at the same depth.
                WirInstr::Seq(seq) => {
                    if !all_br_variant_values_are_struct_new(seq, valid_type_indices, target_depth)
                    {
                        return false;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    true
}

/// Per-function local def/use counts, computed in one walk. Replaces the
/// per-site whole-body rescans (`count_local_set_in_body` /
/// `count_local_get` per temp) that made validation O(n²).
#[derive(Default)]
struct LocalDefUse {
    /// `LocalSet` + `LocalTee` writes per local name.
    sets: crate::hashmap::IndexMap<String, usize>,
    /// `LocalGet` reads per local name.
    gets: crate::hashmap::IndexMap<String, usize>,
}

impl LocalDefUse {
    fn of_body(body: &[WirInstr]) -> Self {
        let mut index = Self::default();
        for instr in body {
            index.scan(instr);
        }
        index
    }

    fn scan(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalGet { name, .. } => {
                *self.gets.entry(name.clone()).or_default() += 1;
            }
            WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
                *self.sets.entry(name.clone()).or_default() += 1;
            }
            _ => {}
        }
        instr.for_each_child(&mut |child| self.scan(child));
    }

    fn set_count(&self, name: &str) -> usize {
        self.sets.get(name).copied().unwrap_or(0)
    }

    fn get_count(&self, name: &str) -> usize {
        self.gets.get(name).copied().unwrap_or(0)
    }
}

/// Read-only context shared by the call-site validation walk over one
/// function body.
struct CallSiteCtx<'a> {
    /// The full function body — temp-local uses are validated across it.
    root_body: &'a [WirInstr],
    candidate_ids: &'a IndexSet<u32>,
    /// Per candidate, the WIR type indices of its payload-bearing case
    /// structs — the only type ids the call-site rewriter's
    /// `case_disc_values` / `field_to_local` maps carry, hence the only
    /// ids a `RefTest` / `RefCast` on the temp may name.
    case_types_by_candidate: &'a crate::hashmap::IndexMap<u32, IndexSet<u32>>,
    /// Def/use counts over `root_body`.
    def_use: &'a LocalDefUse,
    /// True when the function being validated is itself a candidate; only
    /// then is `Return { Some(Call(candidate)) }` a valid (tail-call) shape.
    caller_is_candidate: bool,
}

/// Phase 2: validate that all call sites of candidate functions are SROA-compatible.
///
/// A call site is compatible if:
/// 1. The call result is stored to a temp local via `LocalSet`, and every use
///    of that temp local matches the variant-access patterns
///    (`RefTest`/`RefCast`/`StructGet`).
/// 2. Or, the call appears as `Return { Some(<wrapper>(Call(candidate))) }`
///    *inside another candidate's body* — the surrounding function has its
///    own multi-value signature, so the callee's multi-value results flow
///    through to the surrounding return without an arity mismatch.
fn validate_call_sites(
    module: &WirPackage,
    candidates: &[(u32, SroaCandidate)],
) -> Vec<(u32, SroaCandidate)> {
    let mut candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();

    let case_types_by_candidate: crate::hashmap::IndexMap<u32, IndexSet<u32>> = candidates
        .iter()
        .map(|(id, c)| {
            let case_types: IndexSet<u32> = c
                .variant_info
                .case_type_indices
                .iter()
                .flatten()
                .copied()
                .collect();
            (*id, case_types)
        })
        .collect();

    // Def/use indexes are loop-invariant: validation never mutates bodies.
    let def_use_by_func: Vec<LocalDefUse> = module
        .functions
        .iter()
        .map(|func| {
            func.body
                .as_deref()
                .map(LocalDefUse::of_body)
                .unwrap_or_default()
        })
        .collect();

    // Iterate to a fix-point. Two kinds of cascade need to converge:
    //
    // 1. **Caller-status cascade.** The tail-call rule accepts
    //    `Return { Some(Call(callee)) }` only when the surrounding caller
    //    is *also* a candidate. Invalidating a caller can therefore make a
    //    previously-valid tail-call site become invalid (its
    //    `caller_is_candidate` flips false), which in turn can invalidate
    //    the callee.
    // 2. **Tail-call-target cascade.** Discovery accepted a function `f`
    //    because its returns include `Return { Some(Call(g)) }` and `g`
    //    was a candidate. When `g` gets invalidated mid-validation, `f`'s
    //    return shape no longer holds (the tail-call target is gone), so
    //    `f` must also be invalidated — otherwise `apply_sroa` rewrites
    //    `f`'s signature to multi-value while leaving `Return(Call(g))`
    //    pointing at the still-single-value `g`, producing a Wasm
    //    validator type mismatch. Re-running the discovery shape check
    //    after each round (against an "effective" candidate set that
    //    excludes pending invalidations) closes this cascade.
    let mut invalid: IndexSet<u32> = IndexSet::default();
    loop {
        let mut round_invalid: IndexSet<u32> = IndexSet::default();

        for (i, func) in module.functions.iter().enumerate() {
            let func_id_index = module.defined_func_base + u32::try_from(i).unwrap();
            let caller_is_candidate = candidate_ids.contains(&func_id_index);
            if let Some(body) = &func.body {
                let ctx = CallSiteCtx {
                    root_body: body,
                    candidate_ids: &candidate_ids,
                    case_types_by_candidate: &case_types_by_candidate,
                    def_use: &def_use_by_func[i],
                    caller_is_candidate,
                };
                validate_call_sites_in_body(body, &ctx, &mut round_invalid);
            }
        }

        // Cascade 2: re-check return shapes against the effective set
        // (candidate_ids minus pending round_invalid). Any candidate
        // whose tail-call target was just invalidated loses its return
        // shape and must be invalidated too. Iterate the recheck until
        // it stabilises so transitive cascades are caught.
        loop {
            let mut cascade_changed = false;
            let effective: IndexSet<u32> = candidate_ids
                .iter()
                .copied()
                .filter(|id| !round_invalid.contains(id))
                .collect();
            let mut effective_by_variant: crate::hashmap::IndexMap<u32, IndexSet<u32>> =
                crate::hashmap::IndexMap::default();
            for (id, c) in candidates {
                if effective.contains(id) {
                    effective_by_variant
                        .entry(c.struct_type_idx)
                        .or_default()
                        .insert(*id);
                }
            }
            for (id, c) in candidates {
                if !effective.contains(id) {
                    continue;
                }
                let body = module.functions[c.func_array_idx].body.as_ref().unwrap();
                let empty: IndexSet<u32> = IndexSet::default();
                let tail_set = effective_by_variant
                    .get(&c.struct_type_idx)
                    .unwrap_or(&empty);
                if !all_returns_are_variant_struct_new(body, &c.valid_case_type_indices, tail_set) {
                    round_invalid.insert(*id);
                    cascade_changed = true;
                }
            }
            if !cascade_changed {
                break;
            }
        }

        let mut new_invalid = false;
        for id in &round_invalid {
            if candidate_ids.swap_remove(id) {
                invalid.insert(*id);
                new_invalid = true;
            }
        }
        if !new_invalid {
            break;
        }
    }

    candidates
        .iter()
        .filter(|(id, _)| !invalid.contains(id))
        .map(|(id, c)| (*id, c.clone()))
        .collect()
}

/// Validate call sites of candidate functions within a flat instruction list.
///
/// `ctx.root_body` is the top-level function body — used when checking that
/// the temp local is only accessed via valid patterns across all scopes, not
/// just the current scope. This prevents SROA when a call site is inside a
/// nested block (If/Block) but the temp local is used in the outer scope in
/// a non-StructGet context (e.g. `return temp`).
fn validate_call_sites_in_body(
    instrs: &[WirInstr],
    ctx: &CallSiteCtx,
    invalid: &mut IndexSet<u32>,
) {
    for instr in instrs {
        // Recurse into nested statement-level blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(body, ctx, invalid);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                validate_expr_context(condition, ctx, invalid);
                validate_call_sites_in_body(then_body, ctx, invalid);
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(eb, ctx, invalid);
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(body, ctx, invalid);
            }
            // `Return { Some(<Seq | Block | If>) }` — descend into the body
            // and validate it as if it were a nested statement list.
            // Synthesised code shapes like
            //
            //     return Seq([let __m = call(d), if ref.test(__m, Ok) ... ])
            //
            // (e.g. `JsonSeqAccess::next_element<i64>` after lowering) need
            // this so the inner `LocalSet(__m, Call(candidate))` reaches
            // the standard variant-access call-site check; without it the
            // outer `Return`'s default fallthrough would mark `candidate`
            // as invalid via `find_nested_candidate_calls`.
            //
            // `If` is included for symmetry with the rewriter's
            // `lift_return_into_variant_leaves` which explicitly handles
            // `Return(If(...))`. Without this arm the validator's `_`
            // fallback over-invalidates candidates that the rewriter
            // could otherwise handle.
            //
            // The tail-call shape `Return { Some(<wrapper>(Call(candidate))) }`
            // is still routed through `check_invalid_call_uses` first so
            // that path's args / block-prefix scan runs (and so we don't
            // accidentally double-recurse on the call itself).
            WirInstr::Return { value: Some(v) }
                if unwrap_to_candidate_call(v, ctx.candidate_ids).is_none()
                    && matches!(
                        v.as_ref(),
                        WirInstr::Seq(_) | WirInstr::Block { .. } | WirInstr::If { .. }
                    ) =>
            {
                match v.as_ref() {
                    WirInstr::Seq(body) | WirInstr::Block { body, .. } => {
                        validate_call_sites_in_body(body, ctx, invalid);
                    }
                    WirInstr::If {
                        condition,
                        then_body,
                        else_body,
                        ..
                    } => {
                        validate_expr_context(condition, ctx, invalid);
                        validate_call_sites_in_body(then_body, ctx, invalid);
                        if let Some(eb) = else_body {
                            validate_call_sites_in_body(eb, ctx, invalid);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            // For non-block instructions, check for invalid call uses at this level
            _ => {
                check_invalid_call_uses(instr, ctx, invalid);
            }
        }
    }

    // Check that LocalSet(Call(candidate)) temps are only used via valid
    // variant-access patterns: RefTest(LocalGet(temp)) or
    // StructGet(RefCast(LocalGet(temp))). Use root_body (the full function
    // body) to catch uses of the temp local in outer scopes.
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let Some(func_id_idx) = unwrap_to_candidate_call(value, ctx.candidate_ids)
        {
            // Reject when the local has more than one definition: SROA assumes
            // the temp is exclusively defined by this call. With mutable locals
            // (e.g. `let mut s: String;` assigned in multiple branches), the
            // other definitions would be silently dropped, producing wrong code.
            if ctx.def_use.set_count(name) > 1 {
                invalid.insert(func_id_idx);
                continue;
            }
            // The rewriter replaces the temp's accesses only within the
            // statement list holding the bind (and its subtrees). A use in
            // an outer scope would survive the rewrite and read the deleted
            // temp — reject unless every read is contained in this list.
            let reads_here: usize = instrs.iter().map(|i| count_local_get(i, name)).sum();
            if reads_here != ctx.def_use.get_count(name) {
                invalid.insert(func_id_idx);
                continue;
            }
            let case_types = &ctx.case_types_by_candidate[&func_id_idx];
            if !all_uses_are_variant_access(ctx.root_body, name, case_types, ctx.def_use) {
                invalid.insert(func_id_idx);
            }
        }
    }
}

/// Validation mirror of `recurse_rewrite_call_sites` for expression
/// positions (`If` conditions and similar operand contexts): the rewriter
/// treats `Block` / `Loop` / `Seq` bodies and `If` arms found there as full
/// statement lists, so validation must too — a `?`-desugared
/// `Seq([__m = call(...), if ref.test(__m) ...])` condition is a normal
/// call site, not grounds for invalidation. Any candidate call at a
/// position the rewriter leaves untouched is invalidated.
fn validate_expr_context(expr: &WirInstr, ctx: &CallSiteCtx, invalid: &mut IndexSet<u32>) {
    match expr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            validate_call_sites_in_body(body, ctx, invalid);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            validate_expr_context(condition, ctx, invalid);
            validate_call_sites_in_body(then_body, ctx, invalid);
            if let Some(eb) = else_body {
                validate_call_sites_in_body(eb, ctx, invalid);
            }
        }
        other => {
            if let WirInstr::Call { func_id, .. } = other
                && ctx.candidate_ids.contains(&func_id.index())
            {
                invalid.insert(func_id.index());
            }
            other.for_each_child(&mut |child| validate_expr_context(child, ctx, invalid));
        }
    }
}

/// Count `LocalGet(name)` occurrences in an instruction tree (all positions).
fn count_local_get(instr: &WirInstr, name: &str) -> usize {
    let mut n = 0;
    if let WirInstr::LocalGet { name: g, .. } = instr
        && g == name
    {
        n += 1;
    }
    instr.for_each_child(&mut |c| n += count_local_get(c, name));
    n
}

/// Check that every reference to `local_name` is a shape the call-site
/// rewriter (`replace_variant_accesses` / `collect_refcast_aliases`)
/// replaces:
/// - `RefTest { type_id ∈ case_types, expr: LocalGet(name) }` — discriminant test
/// - `StructGet { expr: RefCast { type_id ∈ case_types, expr: LocalGet(name) } }` — payload access
/// - `LocalSet(alias, RefCast { type_id ∈ case_types, expr: LocalGet(name) })` —
///   cast-alias binding, provided the alias is single-def and read only via
///   `StructGet(LocalGet(alias))`.
///
/// The type-id constraint matters: the rewriter's `case_disc_values` /
/// `field_to_local` maps are keyed by the candidate's payload-bearing case
/// types, so an access naming any other type would survive the rewrite and
/// read the deleted temp.
fn all_uses_are_variant_access(
    instrs: &[WirInstr],
    local_name: &str,
    case_types: &IndexSet<u32>,
    def_use: &LocalDefUse,
) -> bool {
    instrs
        .iter()
        .all(|instr| check_uses_are_variant_access(instr, local_name, case_types, instrs, def_use))
}

fn check_uses_are_variant_access(
    instr: &WirInstr,
    local_name: &str,
    case_types: &IndexSet<u32>,
    root: &[WirInstr],
    def_use: &LocalDefUse,
) -> bool {
    // Discriminant test: `RefTest { case_type, LocalGet(temp) }`.
    if let WirInstr::RefTest { type_id, expr, .. } = instr
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && name == local_name
    {
        return case_types.contains(&type_id.index());
    }
    // Payload read: `StructGet { RefCast { case_type, LocalGet(temp) } }`.
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::RefCast {
            type_id,
            expr: rc_expr,
            ..
        } = expr.as_ref()
        && let WirInstr::LocalGet { name, .. } = rc_expr.as_ref()
        && name == local_name
    {
        return case_types.contains(&type_id.index());
    }
    // Cast-alias binding: `LocalSet(alias, RefCast { case_type, LocalGet(temp) })`.
    // `collect_refcast_aliases` Nops the definition and resolves later
    // `StructGet(LocalGet(alias))` reads through the alias map, so the alias
    // must have no other definition and no other kind of use.
    if let WirInstr::LocalSet { name: alias, value } = instr
        && let WirInstr::RefCast {
            type_id,
            expr: rc_expr,
            ..
        } = value.as_ref()
        && let WirInstr::LocalGet { name, .. } = rc_expr.as_ref()
        && name == local_name
        && alias != local_name
    {
        return case_types.contains(&type_id.index())
            && def_use.set_count(alias) == 1
            && instrs_alias_uses_are_struct_get(root, alias);
    }
    // Any other read or re-definition of the temp disqualifies it. The
    // defining `LocalSet(temp, <call>)` reaches the recursion below, which
    // rejects any use of the temp inside its own RHS.
    match instr {
        WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } if name == local_name => {
            false
        }
        other => {
            let mut ok = true;
            other.for_each_child(&mut |child| {
                if ok
                    && !check_uses_are_variant_access(child, local_name, case_types, root, def_use)
                {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Check that every read of `alias` is `StructGet { expr: LocalGet(alias) }`
/// (the shape `replace_variant_accesses` resolves through the alias map).
fn instrs_alias_uses_are_struct_get(instrs: &[WirInstr], alias: &str) -> bool {
    instrs.iter().all(|i| alias_uses_are_struct_get(i, alias))
}

fn alias_uses_are_struct_get(instr: &WirInstr, alias: &str) -> bool {
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && name == alias
    {
        return true;
    }
    match instr {
        WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } if name == alias => false,
        other => {
            let mut ok = true;
            other.for_each_child(&mut |child| {
                if ok && !alias_uses_are_struct_get(child, alias) {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// One step of the path from a wrapped call-site value to its result leaf.
/// `value_idx` is the index of the value-producing element in the wrapper's
/// instruction list; earlier elements are prefix statements the rewriter
/// hoists out of the wrapper.
#[derive(Clone, Copy)]
enum ResultStep {
    /// `Seq(body)`: value is `body[value_idx]` (the last element).
    Seq { value_idx: usize },
    /// `Block { body }`: value is `body[value_idx]` (the effective last
    /// element after trimming a trailing `Unreachable`).
    Block { value_idx: usize },
    /// A `Seq([.., value, Br(0)])` break-with-value at a `Block`'s result
    /// position: value is `seq[value_idx]` (`seq.len() - 2`).
    BreakValue { value_idx: usize },
}

/// Resolve the value-producing leaf of a call-site RHS through `Block` /
/// `Seq` wrappers, verifying that stripping the wrappers is sound. The one
/// shape definition shared by validation (`unwrap_to_candidate_call`,
/// `unwrap_to_inner_call`, `validate_wrapper_prefixes`) and the
/// rewriter (`take_call_from_local_set`), so accept and rewrite cannot
/// diverge.
///
/// Soundness of stripping requires:
/// - No prefix statement may branch to *any* `Block` frame being stripped
///   — not just the innermost one: a `BrIf { depth: 1 }` in a nested
///   block's prefix targeting the outer block would dangle once both
///   wrappers are removed.
/// - A break-with-value `Br` must target the block it exits (`depth == 0`);
///   a deeper target carries the value elsewhere.
/// - A trailing `Unreachable` after a break-with-value (appended by
///   `translate_stmts_as_value` so the Wasm validator sees no fallthrough
///   value) is dead code — skipped in `Block` bodies only; a `Seq` has no
///   such emitter convention, so its trailing `Unreachable` means "then
///   trap" and blocks unwrapping.
fn resolve_wrapped_result(instr: &WirInstr) -> Option<(Vec<ResultStep>, &WirInstr)> {
    let mut steps = Vec::new();
    let leaf = resolve_wrapped_result_inner(instr, 0, &mut steps)?;
    Some((steps, leaf))
}

fn resolve_wrapped_result_inner<'a>(
    instr: &'a WirInstr,
    stripped_blocks: u32,
    steps: &mut Vec<ResultStep>,
) -> Option<&'a WirInstr> {
    match instr {
        WirInstr::Seq(body) => {
            let (last, prefix) = body.split_last()?;
            if any_branch_targets_enclosing(prefix, stripped_blocks) {
                return None;
            }
            steps.push(ResultStep::Seq {
                value_idx: body.len() - 1,
            });
            resolve_wrapped_result_inner(last, stripped_blocks, steps)
        }
        WirInstr::Block { body, .. } => {
            let effective_len = if matches!(body.last(), Some(WirInstr::Unreachable)) {
                body.len() - 1
            } else {
                body.len()
            };
            let value_idx = effective_len.checked_sub(1)?;
            if any_branch_targets_enclosing(&body[..value_idx], stripped_blocks + 1) {
                return None;
            }
            steps.push(ResultStep::Block { value_idx });
            let last = &body[value_idx];
            if let WirInstr::Seq(seq) = last
                && let Some((WirInstr::Br { depth }, rest)) = seq.split_last()
            {
                if *depth != 0 {
                    return None;
                }
                let value_idx = rest.len().checked_sub(1)?;
                if any_branch_targets_enclosing(&rest[..value_idx], stripped_blocks + 1) {
                    return None;
                }
                steps.push(ResultStep::BreakValue { value_idx });
                return resolve_wrapped_result_inner(&rest[value_idx], stripped_blocks + 1, steps);
            }
            resolve_wrapped_result_inner(last, stripped_blocks + 1, steps)
        }
        other => Some(other),
    }
}

/// True when any instruction in `instrs` branches to one of the `frame_count`
/// innermost enclosing block frames (relative depths `0..frame_count` at this
/// position; nested control frames shift the window by one per level).
fn any_branch_targets_enclosing(instrs: &[WirInstr], frame_count: u32) -> bool {
    if frame_count == 0 {
        return false;
    }
    instrs
        .iter()
        .any(|instr| instr_branches_into_range(instr, 0, frame_count))
}

fn instr_branches_into_range(instr: &WirInstr, base: u32, count: u32) -> bool {
    let in_range = |d: u32| d >= base && d - base < count;
    match instr {
        WirInstr::Br { depth } => in_range(*depth),
        WirInstr::BrIf { depth, condition } => {
            in_range(*depth) || instr_branches_into_range(condition, base, count)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets.iter().copied().any(in_range)
                || in_range(*default)
                || instr_branches_into_range(index, base, count)
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            instr_branches_into_range(condition, base, count)
                || then_body
                    .iter()
                    .any(|i| instr_branches_into_range(i, base + 1, count))
                || else_body.as_ref().is_some_and(|eb| {
                    eb.iter()
                        .any(|i| instr_branches_into_range(i, base + 1, count))
                })
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => body
            .iter()
            .any(|i| instr_branches_into_range(i, base + 1, count)),
        WirInstr::Seq(items) => items
            .iter()
            .any(|i| instr_branches_into_range(i, base, count)),
        // Other instructions cannot introduce a label, but their operands can
        // still embed control flow (e.g. a `BranchHint`-wrapped condition or a
        // labeled block in value position), so recurse at the same depth — the
        // label-introducing arms above adjust it where needed.
        other => {
            let mut found = false;
            other.for_each_child(&mut |child| {
                found = found || instr_branches_into_range(child, base, count);
            });
            found
        }
    }
}

/// Look through trivial `Block` / `Seq` wrappers to find a `Call` to a
/// candidate function at the result position. Returns the `func_id` index.
///
/// `LocalSet`-from-`Call` site-effects are often wrapped in a `Seq` in WIR —
/// e.g. `LocalSet(name, Seq([prefix..., Call(f)]))` — and inlining wraps
/// results in labeled `Block`s. Without unwrapping,
/// `find_nested_candidate_calls` would mis-classify every such site as a
/// "nested candidate call" and invalidate `f`, even though the pattern is the
/// idiomatic LocalSet-bound call we want to support.
fn unwrap_to_candidate_call(instr: &WirInstr, candidate_ids: &IndexSet<u32>) -> Option<u32> {
    match resolve_wrapped_result(instr)?.1 {
        WirInstr::Call { func_id, .. } if candidate_ids.contains(&func_id.index()) => {
            Some(func_id.index())
        }
        _ => None,
    }
}

/// Check if an instruction uses a candidate call result in an invalid way.
/// Invalid: Call to candidate as a nested expression (not direct child of
/// `LocalSet` or `Return` in a candidate caller).
fn check_invalid_call_uses(instr: &WirInstr, ctx: &CallSiteCtx, invalid: &mut IndexSet<u32>) {
    match instr {
        // LocalSet { value: <wrapper>(Call) } is valid — handled separately
        WirInstr::LocalSet { value, .. }
            if unwrap_to_candidate_call(value, ctx.candidate_ids).is_some() =>
        {
            // Check args of the underlying call for nested candidate calls
            if let Some(WirInstr::Call { args, .. }) = unwrap_to_inner_call(value) {
                for arg in args {
                    find_nested_candidate_calls(arg, ctx.candidate_ids, invalid);
                }
            }
            // Also check prefix instructions in any block wrapper.
            // When the call is wrapped in Block { body: [prefix..., result_call] },
            // the prefix instructions may themselves contain SROA-compatible
            // `LocalSet(temp, Call(candidate))` shapes that should be accepted,
            // not invalidated. The prefix walk reuses the standard call-site
            // validator so those patterns are recognised.
            validate_wrapper_prefixes(value, ctx, invalid);
        }
        // `Return { Some(<wrapper>(Call(candidate))) }` is the tail-call
        // shape accepted by the fix-point candidate discovery. After the
        // callee's signature is rewritten to multi-value, the Call already
        // pushes the correct number of values for the surrounding Return.
        // *But only when the surrounding function is also a candidate* —
        // otherwise the caller's still-single-value `Return` would mismatch
        // the callee's new multi-value sig. We still need to scan the call's
        // arguments and any block prefix for nested candidate calls that the
        // rewrite cannot reach.
        WirInstr::Return { value: Some(v) }
            if ctx.caller_is_candidate
                && unwrap_to_candidate_call(v, ctx.candidate_ids).is_some() =>
        {
            if let Some(WirInstr::Call { args, .. }) = unwrap_to_inner_call(v) {
                for arg in args {
                    find_nested_candidate_calls(arg, ctx.candidate_ids, invalid);
                }
            }
            validate_wrapper_prefixes(v, ctx, invalid);
        }
        // A `LocalSet`/`LocalTee` whose value is a compound `Seq`/`Block`/`If`,
        // and whose result is not itself a candidate call (the first arm
        // handles that), can still nest a clean `LocalSet(temp, Call(candidate))`
        // in its prefix or branches: `let x = next_element()?` desugars to the
        // call bound to a temp one level down inside a `Seq`. The old `_` arm
        // invalidated those blindly, keeping every `seq.next_element()?` boxed.
        // Recurse into the sub-body so the inner temp gets normal variant-access
        // validation, matching the rewriter's `recurse_rewrite_call_sites` which
        // descends here too (accept and rewrite must agree). A truly unrewritable
        // nested call (a bare `Call` branch value with no binding temp) still
        // reaches the catch-all below and is rejected.
        WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. }
            if matches!(
                value.as_ref(),
                WirInstr::Seq(_) | WirInstr::Block { .. } | WirInstr::If { .. }
            ) =>
        {
            match value.as_ref() {
                WirInstr::Seq(body) | WirInstr::Block { body, .. } => {
                    validate_call_sites_in_body(body, ctx, invalid);
                }
                WirInstr::If {
                    condition,
                    then_body,
                    else_body,
                    ..
                } => {
                    validate_expr_context(condition, ctx, invalid);
                    validate_call_sites_in_body(then_body, ctx, invalid);
                    if let Some(eb) = else_body {
                        validate_call_sites_in_body(eb, ctx, invalid);
                    }
                }
                _ => unreachable!(),
            }
        }
        // Any other instruction that contains a Call to a candidate is invalid
        _ => {
            find_nested_candidate_calls(instr, ctx.candidate_ids, invalid);
        }
    }
}

/// Validate the prefix statements of every `Block` / `Seq` wrapper level on
/// the path to a call-site result. The prefixes form their own
/// statement-list scopes and may themselves contain SROA-compatible
/// `LocalSet(temp, Call(candidate))` sites; the matching rewriter
/// (`take_call_from_local_set` → `rewrite_call_sites` on the extracted
/// prefix) hoists and rewrites exactly these slices, so validating them with
/// the standard call-site validator keeps accept and rewrite aligned.
fn validate_wrapper_prefixes(instr: &WirInstr, ctx: &CallSiteCtx, invalid: &mut IndexSet<u32>) {
    let Some((steps, _)) = resolve_wrapped_result(instr) else {
        return;
    };
    let mut current = instr;
    for step in steps {
        let (list, value_idx) = match (step, current) {
            (
                ResultStep::Seq { value_idx } | ResultStep::BreakValue { value_idx },
                WirInstr::Seq(body),
            )
            | (ResultStep::Block { value_idx }, WirInstr::Block { body, .. }) => (body, value_idx),
            _ => unreachable!("resolve_wrapped_result path mismatch"),
        };
        validate_call_sites_in_body(&list[..value_idx], ctx, invalid);
        current = &list[value_idx];
    }
}

/// Unwrap through `Block` / `Seq` wrappers to the inner `Call` instruction
/// (for arg checking). Shares [`resolve_wrapped_result`] with the candidate
/// check and the rewriter, so it sees exactly the call they see — including
/// through a trailing `Unreachable` after a break-with-value.
fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match resolve_wrapped_result(instr) {
        Some((_, call @ WirInstr::Call { .. })) => Some(call),
        _ => None,
    }
}

/// Recursively find calls to candidate functions nested in expressions.
fn find_nested_candidate_calls(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Call { func_id, .. } = instr
        && candidate_ids.contains(&func_id.index())
    {
        invalid.insert(func_id.index());
    }
    instr.for_each_child(&mut |child| find_nested_candidate_calls(child, candidate_ids, invalid));
}

/// Phase 3: rewrite confirmed functions to the multi-value ABI — new func
/// types and signatures, `Return` bodies lowered to flat result vectors,
/// and every call site rebound through `MultiValueLocalBind`.
fn apply_sroa(module: &mut WirPackage, confirmed: &[(u32, SroaCandidate)]) {
    // Build a lookup from func_id_index → candidate info
    let candidate_map: crate::hashmap::IndexMap<u32, &SroaCandidate> =
        confirmed.iter().map(|(id, c)| (*id, c)).collect();
    let candidate_ids: IndexSet<u32> = candidate_map.keys().copied().collect();

    // Step A: Create new func types and rewrite function signatures + bodies.
    for (_func_id_index, candidate) in confirmed {
        let func = &mut module.functions[candidate.func_array_idx];

        // Create new func type with multi-value results
        let old_type_idx = func.type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: old_func_type.params.clone(),
            results: candidate.field_types.clone(),
        };

        // Add the new func type to the module types
        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));

        // Update the function's type_id
        let new_type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
        func.type_id = new_type_id;

        // Rewrite returns in the body: StructNew → Seq of field values
        if let Some(body) = &mut func.body {
            compiler_trace!(
                "sroa_variant_return",
                "applying SROA to function {}",
                func.name
            );
            rewrite_variant_returns_to_multi_value(
                body,
                &candidate.variant_info,
                &candidate.field_types,
            );
        }
    }

    // Step B: Rewrite call sites in ALL function bodies.
    // Use indexed access to split borrows between module.types and module.functions.
    for i in 0..module.functions.len() {
        if module.functions[i].body.is_some() {
            let body = module.functions[i].body.as_mut().unwrap();
            rewrite_call_sites(body, &candidate_map, &candidate_ids, &module.types);
        }
    }
}

/// Rewrite variant returns to multi-value.
///
/// Transforms `Return { StructNew { type_id: case_type, fields } }` into
/// `Return { Seq([discriminant, payload_0, ..., default_padding...]) }`.
/// Unit cases (no payload) get default values (0/0.0/ref.null) for payload slots.
/// Also handles `return match { ... }` where the return value is a complex expression.
fn rewrite_variant_returns_to_multi_value(
    instrs: &mut [WirInstr],
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) } => match v.as_ref() {
                WirInstr::StructNew { .. } => {
                    if let WirInstr::StructNew { type_id, fields } =
                        std::mem::replace(v.as_mut(), WirInstr::Nop)
                    {
                        let mut padded =
                            pad_variant_fields(fields, vi, result_types, type_id.index());
                        // Recurse into padded fields: they may contain nested Return { StructNew }
                        // from early returns inside struct field expressions.
                        rewrite_variant_returns_to_multi_value(&mut padded, vi, result_types);
                        **v = WirInstr::Seq(padded);
                    }
                }
                WirInstr::Seq(_) | WirInstr::If { .. } | WirInstr::Block { .. } => {
                    let mut value_expr = std::mem::replace(v.as_mut(), WirInstr::Nop);
                    lift_return_into_variant_leaves(&mut value_expr, vi, result_types);
                    *instr = value_expr;
                }
                _ => {}
            },
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                rewrite_variant_returns_to_multi_value(body, vi, result_types);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                // The condition is an expression that can itself contain a
                // `Return` — e.g. `if expr? == x { … }`, where the `?` desugar
                // puts a `return Err(…)` inside the condition. Those returns
                // must be rewritten to the multi-value shape too, or the
                // function's signature and a stray boxed return disagree.
                rewrite_variant_returns_to_multi_value(
                    std::slice::from_mut(condition.as_mut()),
                    vi,
                    result_types,
                );
                rewrite_variant_returns_to_multi_value(then_body, vi, result_types);
                if let Some(eb) = else_body {
                    rewrite_variant_returns_to_multi_value(eb, vi, result_types);
                }
            }
            WirInstr::Seq(body) => {
                rewrite_variant_returns_to_multi_value(body, vi, result_types);
            }
            WirInstr::Drop(inner) => {
                // Drop wraps an expression whose value is discarded.
                // If the inner expression is fully divergent (all paths return),
                // the Drop never executes. We must unwrap it because the inner
                // expression no longer produces a value after return rewriting.
                // If not divergent (e.g., drop(call(...))), keep the Drop.
                if inner.always_diverges() {
                    let mut unwrapped = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    clear_result_types_on_divergent(&mut unwrapped);
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(&mut unwrapped),
                        vi,
                        result_types,
                    );
                    *instr = unwrapped;
                } else {
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(inner.as_mut()),
                        vi,
                        result_types,
                    );
                }
            }
            // For all other instructions (LocalSet, Drop-wrapped values,
            // etc.), recurse into any nested children that might contain
            // Return.
            other => {
                other.for_each_boxed_child_mut(&mut |child| {
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(child),
                        vi,
                        result_types,
                    );
                });
            }
        }
    }
}

/// Clear result types on If/Block nodes that are fully divergent,
/// so they don't declare values that are never produced.
fn clear_result_types_on_divergent(instr: &mut WirInstr) {
    match instr {
        WirInstr::If {
            result,
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    clear_result_types_on_divergent(child);
                }
            }
            if then_body.iter().any(WirInstr::always_diverges)
                && else_body
                    .as_ref()
                    .is_some_and(|eb| eb.iter().any(WirInstr::always_diverges))
            {
                *result = None;
            }
        }
        WirInstr::Block { result, body, .. } => {
            for child in body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
            // A statement that diverges kills the fallthrough value, but a
            // `Br(0)` still delivers the block's typed value via a
            // `[value, Br(0)]` exit pair (`always_diverges` deliberately
            // never treats `Block` as divergent for the same reason).
            // Clear the result only when no branch targets this block.
            if body.iter().any(WirInstr::always_diverges) && !any_branch_targets_enclosing(body, 1)
            {
                *result = None;
            }
        }
        WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
        }
        WirInstr::Drop(inner) => {
            clear_result_types_on_divergent(inner);
        }
        _ => {}
    }
}

/// Pad variant fields with default values for missing payload slots.
/// Also replaces `Nop` fields (unit/void placeholders from `StructNew`) with
/// appropriate default values, since Nop produces no value in flat multi-value returns.
///
/// `case_type_idx`: the WIR type index of the case struct being constructed (needed for
/// per-case slot layout to determine which slots this case's payloads go into).
fn pad_variant_fields(
    fields: Vec<WirInstr>,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
    case_type_idx: u32,
) -> Vec<WirInstr> {
    if let Some(ref offsets) = vi.case_slot_offsets {
        // Per-case slot layout: each case has dedicated payload slots.
        // Find which case this is by matching case_type_idx.
        let disc_expr = fields[0].clone();
        let payload_exprs: Vec<WirInstr> = fields.into_iter().skip(1).collect();

        // Find the case index for this type_id
        let case_idx = vi
            .case_type_indices
            .iter()
            .position(|opt| opt.as_ref() == Some(&case_type_idx));

        // Build the full result: [disc, slot0, slot1, ..., slotN]
        let total_payload_slots = result_types.len() - 1;
        let mut result = Vec::with_capacity(result_types.len());
        result.push(disc_expr);

        // Initialize all payload slots with defaults
        for slot in 0..total_payload_slots {
            result.push(default_value_for_type(&result_types[1 + slot]));
        }

        // Place this case's payloads in their dedicated slots
        if let Some(ci) = case_idx {
            let offset = offsets[ci];
            for (pi, payload) in payload_exprs.into_iter().enumerate() {
                let payload = if matches!(payload, WirInstr::Nop) {
                    default_value_for_type(&result_types[1 + offset + pi])
                } else {
                    payload
                };
                result[1 + offset + pi] = payload;
            }
        }
        // else: unit case (no payloads), all defaults are correct

        result
    } else {
        // Homogeneous layout (original behavior)
        let payload_count = fields.len() - 1; // subtract discriminant
        let mut new_fields = fields;
        // Replace any Nop payload fields with default values for their type position
        for (i, field) in new_fields.iter_mut().enumerate().skip(1) {
            if matches!(field, WirInstr::Nop) {
                let pos = i - 1; // payload position (skip discriminant)
                if pos < result_types.len() - 1 {
                    *field = default_value_for_type(&result_types[1 + pos]);
                }
            }
        }
        for pos in payload_count..vi.max_payload_count {
            let ty = &result_types[1 + pos]; // +1 to skip discriminant
            new_fields.push(default_value_for_type(ty));
        }
        new_fields
    }
}

/// Lift `Return` into leaf `StructNew` positions for variant SROA.
fn lift_return_into_variant_leaves(
    expr: &mut WirInstr,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    match expr {
        WirInstr::StructNew { .. } => {
            if let WirInstr::StructNew { type_id, fields } = std::mem::replace(expr, WirInstr::Nop)
            {
                *expr = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                        fields,
                        vi,
                        result_types,
                        type_id.index(),
                    )))),
                };
            }
        }
        WirInstr::Seq(items) => {
            if let Some((last, prefix)) = items.split_last_mut() {
                // Only the last item is in value (tail) position; the prefix
                // statements may still contain `return` statements (e.g. a
                // `?`-desugared early `return Err(…)` nested in a `let x = if …`
                // binding). Those must be rewritten to the multi-value shape.
                rewrite_variant_returns_to_multi_value(prefix, vi, result_types);
                lift_return_into_variant_leaves(last, vi, result_types);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            result,
        } => {
            *result = None;
            // A `?` in the condition desugars to a `return Err(…)` embedded in
            // the condition expression; rewrite those too.
            rewrite_variant_returns_to_multi_value(
                std::slice::from_mut(condition.as_mut()),
                vi,
                result_types,
            );
            if let Some((last, prefix)) = then_body.split_last_mut() {
                rewrite_variant_returns_to_multi_value(prefix, vi, result_types);
                lift_return_into_variant_leaves(last, vi, result_types);
            }
            if let Some(eb) = else_body
                && let Some((last, prefix)) = eb.split_last_mut()
            {
                rewrite_variant_returns_to_multi_value(prefix, vi, result_types);
                lift_return_into_variant_leaves(last, vi, result_types);
            }
        }
        WirInstr::Block { body, result, .. } => {
            if result.is_some() {
                // The block's value flows through `[StructNew, Br]` exits and
                // the fallthrough tail; non-tail statements may still hold
                // `return` statements that need the multi-value rewrite.
                if let Some((_, prefix)) = body.split_last_mut() {
                    rewrite_variant_returns_to_multi_value(prefix, vi, result_types);
                }
                rewrite_variant_struct_new_br_to_return(body, 0, vi, result_types);
                *result = None;
            }
        }
        _ => {}
    }
}

/// Variant version of `rewrite_struct_new_br_to_return`.
fn rewrite_variant_struct_new_br_to_return(
    instrs: &mut [WirInstr],
    target_depth: u32,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    let mut i = 0;
    while i + 1 < instrs.len() {
        if matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth) {
            if matches!(&instrs[i], WirInstr::StructNew { .. }) {
                if let WirInstr::StructNew { type_id, fields } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
                {
                    instrs[i] = WirInstr::Return {
                        value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                            fields,
                            vi,
                            result_types,
                            type_id.index(),
                        )))),
                    };
                }
                instrs[i + 1] = WirInstr::Nop;
            }
            i += 2;
        } else {
            match &mut instrs[i] {
                // `Loop` adds a depth level like `Block`; recurse so a
                // `[StructNew, Br]` exit nested in a loop is lowered too, keeping
                // this rewriter symmetric with `all_br_variant_values_are_struct_new`.
                WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                    rewrite_variant_struct_new_br_to_return(
                        body,
                        target_depth + 1,
                        vi,
                        result_types,
                    );
                }
                WirInstr::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    rewrite_variant_struct_new_br_to_return(
                        then_body,
                        target_depth + 1,
                        vi,
                        result_types,
                    );
                    if let Some(eb) = else_body {
                        rewrite_variant_struct_new_br_to_return(
                            eb,
                            target_depth + 1,
                            vi,
                            result_types,
                        );
                    }
                }
                WirInstr::Seq(items) => {
                    rewrite_variant_struct_new_br_to_return(items, target_depth, vi, result_types);
                }
                _ => {}
            }
            i += 1;
        }
    }

    // Handle fallthrough.
    // The `matches!` guard before `mem::replace` is intentional to avoid replacing
    // non-StructNew instructions with Nop.
    if let Some(last) = instrs.last_mut() {
        if matches!(last, WirInstr::StructNew { .. })
            && let WirInstr::StructNew { type_id, fields } = std::mem::replace(last, WirInstr::Nop)
        {
            *last = WirInstr::Return {
                value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                    fields,
                    vi,
                    result_types,
                    type_id.index(),
                )))),
            };
        } else if let WirInstr::Seq(items) = last {
            rewrite_variant_struct_new_br_to_return(items, target_depth, vi, result_types);
        }
    }
}

/// Produce a default (zero) value for a given WIR type, used to pad the
/// result-vector slots a variant case leaves unused. Exhaustive over every
/// type [`is_eligible_field_type`] admits — a wrong-typed pad is invalid
/// Wasm, so an unpaddable type is a bug in the eligibility whitelist.
fn default_value_for_type(ty: &WirType) -> WirInstr {
    match ty {
        WirType::I32
        | WirType::I8
        | WirType::I16
        | WirType::U8
        | WirType::U16
        | WirType::U32
        | WirType::Bool
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. } => WirInstr::I32Const(0),
        WirType::I64 | WirType::U64 => WirInstr::I64Const(0),
        WirType::F32 => WirInstr::F32Const(0.0),
        WirType::F64 => WirInstr::F64Const(0.0),
        WirType::V128 => WirInstr::V128Const(0),
        WirType::Ref { .. } | WirType::AbstractRef { .. } => WirInstr::RefNull {
            heap_type: crate::wir::WirAbstractHeapType::None,
        },
        WirType::Unit => panic!("variant SROA cannot pad a unit-typed result slot"),
    }
}

/// Build the call-site [`VariantReplacement`] for a variant whose result vector
/// fields are bound to the locals in `field_map` (keyed by the layout's field
/// names — `"discriminant"`, `"payload_N"` or `"caseN_payload_M"`).
///
/// Maps each case struct's `(case_type_idx, field_name)` to the local carrying
/// that field, and records which payload locals hold non-nullable refs (they
/// need `ref.as_non_null` when read). Shared by the function-return rewriter
/// ([`rewrite_call_sites`]) and the nested-slot flattener
/// ([`flatten_variant_slots`]), so both derive the exact same replacement.
fn build_variant_replacement(
    field_map: &crate::hashmap::IndexMap<String, String>,
    vi: &VariantSroaInfo,
    variant_type_idx: u32,
    types: &[WirTypeDef],
) -> VariantReplacement {
    let disc_local = field_map["discriminant"].clone();
    let mut case_disc_values: crate::hashmap::IndexMap<u32, i32> =
        crate::hashmap::IndexMap::default();
    let mut field_to_local: crate::hashmap::IndexMap<(u32, String), String> =
        crate::hashmap::IndexMap::default();

    for (disc_val, case_type_opt) in vi.case_type_indices.iter().enumerate() {
        if let Some(case_type_idx) = case_type_opt {
            case_disc_values.insert(*case_type_idx, i32::try_from(disc_val).unwrap());

            // Look up the case struct type to map field names → sroa locals
            if let Some(WirTypeDef::Struct(st)) = types.get(*case_type_idx as usize) {
                for (field_pos, field) in st.fields.iter().enumerate() {
                    if field_pos == 0 {
                        // Discriminant field
                        field_to_local
                            .insert((*case_type_idx, field.name.clone()), disc_local.clone());
                    } else {
                        let payload_idx = field_pos - 1;
                        // For per-case layout, slot names are
                        // "case{disc_val}_payload_{idx}"; for shared layout,
                        // "payload_{idx}".
                        let payload_name = if vi.case_slot_offsets.is_some() {
                            format!("case{disc_val}_payload_{payload_idx}")
                        } else {
                            format!("payload_{payload_idx}")
                        };
                        if let Some(sroa_local) = field_map.get(&payload_name) {
                            field_to_local
                                .insert((*case_type_idx, field.name.clone()), sroa_local.clone());
                        }
                    }
                }
            }
        }
    }

    // Track which SROA locals hold ref types that need ref.as_non_null when read.
    // The check must be against the ORIGINAL variant-case payload type from
    // `WirVariantCase::payload`, not the case struct's field type: the latter
    // is always declared nullable for the Option<&T> boxing optimisation,
    // which loses the information that a `Some(non_null_ref)` payload is
    // semantically non-null at the Wado source level.
    let mut ref_locals = crate::hashmap::IndexSet::default();
    if let Some(WirTypeDef::Variant(wv)) = types.get(variant_type_idx as usize) {
        for (disc_val_2, case_type_opt_2) in vi.case_type_indices.iter().enumerate() {
            if case_type_opt_2.is_none() {
                continue;
            }
            // Locate the corresponding variant case by discriminant value.
            let Some(wir_case) = wv.cases.iter().find(|c| c.index as usize == disc_val_2) else {
                continue;
            };
            for (payload_idx, payload_ty) in wir_case.payload.iter().enumerate() {
                let is_non_nullable_ref = matches!(
                    payload_ty,
                    WirType::Ref {
                        nullable: false,
                        ..
                    }
                );
                if !is_non_nullable_ref {
                    continue;
                }
                let payload_name = if vi.case_slot_offsets.is_some() {
                    format!("case{disc_val_2}_payload_{payload_idx}")
                } else {
                    format!("payload_{payload_idx}")
                };
                if let Some(sroa_local) = field_map.get(&payload_name) {
                    ref_locals.insert(sroa_local.clone());
                }
            }
        }
    }

    VariantReplacement {
        disc_local,
        case_disc_values,
        field_to_local,
        ref_locals,
    }
}

/// Rewrite call sites of SROA'd functions.
///
/// For each `LocalSet { name: T, value: Call { func_id } }` where `func_id` is SROA'd:
/// 1. Replace the `LocalSet` with `MultiValueLocalBind` that binds results to fresh locals.
/// 2. For struct candidates: replace `StructGet { field, expr: LocalGet(T) }` with `LocalGet`.
/// 3. For variant candidates: replace `RefTest` with `I32Eq` and
///    `StructGet { RefCast { LocalGet(T) } }` with `LocalGet`.
fn rewrite_call_sites(
    instrs: &mut Vec<WirInstr>,
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
    candidate_ids: &IndexSet<u32>,
    types: &[WirTypeDef],
) {
    // Variant replacements: temp_name → VariantReplacement
    let mut variant_replacements: crate::hashmap::IndexMap<String, VariantReplacement> =
        crate::hashmap::IndexMap::default();

    // First pass: find call sites and prepare MultiValueLocalBind + replacement map
    let mut result = Vec::with_capacity(instrs.len());
    let mut i = 0;

    while i < instrs.len() {
        // Skip optional DeclareLocal before the LocalSet
        let set_idx = match &instrs[i] {
            WirInstr::DeclareLocal { name: dn, .. } if i + 1 < instrs.len() => {
                if is_candidate_call_set(&instrs[i + 1], dn, candidate_ids) {
                    i + 1
                } else {
                    result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                    i += 1;
                    continue;
                }
            }
            _ => i,
        };

        // Check if this is a LocalSet wrapping a Call to a candidate
        let Some((func_id_idx, temp_name)) =
            extract_candidate_call_info(&instrs[set_idx], candidate_ids)
        else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };

        let candidate = candidate_map[&func_id_idx];

        // Generate fresh local names for each field and declare them
        let mut field_map: crate::hashmap::IndexMap<String, String> =
            crate::hashmap::IndexMap::default();
        let mut locals: Vec<Option<String>> = Vec::with_capacity(candidate.field_count);
        for (fi, field_name) in candidate.field_names.iter().enumerate() {
            let fresh = format!("__sroa_{temp_name}_{field_name}");
            field_map.insert(field_name.clone(), fresh.clone());
            // Emit DeclareLocal for the fresh local with the field's type
            result.push(WirInstr::DeclareLocal {
                name: fresh.clone(),
                ty: candidate.field_types[fi].clone(),
            });
            locals.push(Some(fresh));
        }

        variant_replacements.insert(
            temp_name,
            build_variant_replacement(
                &field_map,
                &candidate.variant_info,
                candidate.struct_type_idx,
                types,
            ),
        );

        // Extract the Call instruction (and any prefix statements from block wrappers)
        let (mut prefix_instrs, call_instr) = take_call_from_local_set(&mut instrs[set_idx]);
        // Recursively rewrite the prefix so any nested
        // `LocalSet(temp, Call(candidate))` shapes it contains are also
        // turned into `MultiValueLocalBind` (and their variant-access
        // sites replaced). The validator's `validate_wrapper_prefixes`
        // accepts these patterns, so the rewriter must follow through —
        // otherwise the inner candidate's signature would be multi-value
        // while its call site stays single-value, causing a Wasm arity
        // mismatch.
        rewrite_call_sites(&mut prefix_instrs, candidate_map, candidate_ids, types);
        // Emit prefix instructions (e.g. local initialization from inlined blocks)
        result.extend(prefix_instrs);
        result.push(WirInstr::MultiValueLocalBind {
            instr: call_instr,
            locals,
        });

        i = set_idx + 1;
    }

    *instrs = result;

    if variant_replacements.is_empty() {
        // Recurse into nested blocks even if no replacements at this level
        for instr in instrs.iter_mut() {
            recurse_rewrite_call_sites(instr, candidate_map, candidate_ids, types);
        }
        return;
    }

    // Second pass: replace variant access patterns.
    {
        // Collect RefCast aliases: `LocalSet { cast_var, RefCast { type_id, LocalGet(temp) } }`
        // where `temp` is a variant-SROA'd local. After copy propagation, `ref.cast` may
        // reference the SROA temp directly but be stored to an intermediate local, with a
        // separate `StructGet { field, LocalGet(cast_var) }` reading the payload.
        let mut refcast_aliases: crate::hashmap::IndexMap<String, (String, u32)> =
            crate::hashmap::IndexMap::default();
        collect_refcast_aliases(instrs, &variant_replacements, &mut refcast_aliases);

        for instr in instrs.iter_mut() {
            replace_variant_accesses(instr, &variant_replacements, &refcast_aliases);
        }
    }

    // Recurse into nested blocks
    for instr in instrs.iter_mut() {
        recurse_rewrite_call_sites(instr, candidate_map, candidate_ids, types);
    }
}

/// Recurse into nested instruction bodies for call site rewriting.
/// [`validate_expr_context`] is the validation mirror of this walk — keep
/// the two in sync so accept and rewrite agree on which positions are
/// statement lists.
fn recurse_rewrite_call_sites(
    instr: &mut WirInstr,
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
    candidate_ids: &IndexSet<u32>,
    types: &[WirTypeDef],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_call_sites(body, candidate_map, candidate_ids, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            recurse_rewrite_call_sites(condition, candidate_map, candidate_ids, types);
            rewrite_call_sites(then_body, candidate_map, candidate_ids, types);
            if let Some(eb) = else_body {
                rewrite_call_sites(eb, candidate_map, candidate_ids, types);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_call_sites(body, candidate_map, candidate_ids, types);
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                recurse_rewrite_call_sites(child, candidate_map, candidate_ids, types);
            });
        }
    }
}
/// Produce a `LocalGet` for an SROA local, wrapping with `RefAsNonNull` if the local
/// holds a nullable ref type (variant SROA payload locals use nullable types for padding).
fn sroa_local_get(
    local_name: &str,
    ref_locals: &crate::hashmap::IndexSet<String>,
    result_ty: crate::wir::WirType,
) -> WirInstr {
    if ref_locals.contains(local_name) {
        // Set the LocalGet's own result type to nullable so downstream
        // cleanup passes don't strip the RefAsNonNull wrapper as
        // redundant. The wrapper is what narrows to the non-null
        // `result_ty` expected by the surrounding consumer (e.g., the
        // callee's non-null `ref T` parameter), after the variant case
        // test has already proved the payload is non-null at runtime.
        let nullable_ty = match &result_ty {
            crate::wir::WirType::Ref { type_id, .. } => crate::wir::WirType::Ref {
                type_id: type_id.clone(),
                nullable: true,
            },
            crate::wir::WirType::AbstractRef { heap_type, .. } => {
                crate::wir::WirType::AbstractRef {
                    heap_type: heap_type.clone(),
                    nullable: true,
                }
            }
            _ => result_ty.clone(),
        };
        let get = WirInstr::LocalGet {
            name: local_name.to_string(),
            result_ty: nullable_ty,
        };
        WirInstr::RefAsNonNull(Box::new(get))
    } else {
        WirInstr::LocalGet {
            name: local_name.to_string(),
            result_ty,
        }
    }
}

/// Collect `RefCast` aliases: find `LocalSet { cast_var, RefCast { type_id, LocalGet(temp) } }`
/// patterns where `temp` is a variant-SROA'd local, and replace them with Nop.
/// The alias map records `cast_var → (temp, type_id_index)` so that later
/// `StructGet { field, LocalGet(cast_var) }` can be resolved through the alias.
fn collect_refcast_aliases(
    instrs: &mut [WirInstr],
    variant_replacements: &crate::hashmap::IndexMap<String, VariantReplacement>,
    aliases: &mut crate::hashmap::IndexMap<String, (String, u32)>,
) {
    for instr in instrs.iter_mut() {
        if let WirInstr::LocalSet { name, value } = instr
            && let WirInstr::RefCast {
                type_id,
                expr: rc_expr,
                ..
            } = value.as_ref()
            && let WirInstr::LocalGet {
                name: temp_name, ..
            } = rc_expr.as_ref()
            && variant_replacements.contains_key(temp_name.as_str())
        {
            aliases.insert(name.clone(), (temp_name.clone(), type_id.index()));
            *instr = WirInstr::Nop;
            continue;
        }
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                collect_refcast_aliases(body, variant_replacements, aliases);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                collect_refcast_aliases(then_body, variant_replacements, aliases);
                if let Some(eb) = else_body {
                    collect_refcast_aliases(eb, variant_replacements, aliases);
                }
            }
            WirInstr::Seq(body) => {
                collect_refcast_aliases(body, variant_replacements, aliases);
            }
            _ => {}
        }
    }
}

/// Replace variant access patterns with scalar local accesses for variant SROA'd temps.
///
/// Handles five patterns:
/// 1. `RefTest { type_id, expr: LocalGet(temp) }` → `I32Eq(LocalGet(disc), I32Const(case_disc))`
/// 2. `StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }` → `LocalGet(sroa_local)`
/// 3. `RefAsNonNull(StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } })` → same
/// 4. `StructGet { field, expr: LocalGet(cast_alias) }` where `cast_alias` was a `RefCast` alias → same
fn replace_variant_accesses(
    instr: &mut WirInstr,
    variant_replacements: &crate::hashmap::IndexMap<String, VariantReplacement>,
    refcast_aliases: &crate::hashmap::IndexMap<String, (String, u32)>,
) {
    // Pattern 3: `RefAsNonNull(StructGet(RefCast(LocalGet(temp))))` — the
    // variant-payload extraction form emitted by `wir_build::pattern_match`.
    // Replaces with `sroa_local_get`, which applies a non-null narrowing when
    // the original variant payload field was non-nullable.
    if let WirInstr::RefAsNonNull(inner) = instr
        && let WirInstr::StructGet {
            field_name,
            expr: sg_expr,
            result_ty,
            ..
        } = inner.as_ref()
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Pattern 1: RefTest { type_id, expr: LocalGet(temp) }
    if let WirInstr::RefTest { type_id, expr, .. } = instr
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
        && let Some(&disc_val) = vr.case_disc_values.get(&type_id.index())
    {
        *instr = WirInstr::I32Eq(
            Box::new(WirInstr::LocalGet {
                name: vr.disc_local.clone(),
                result_ty: crate::wir::WirType::I32,
            }),
            Box::new(WirInstr::I32Const(disc_val)),
        );
        return;
    }

    // Pattern 2: StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }
    if let WirInstr::StructGet {
        field_name,
        expr: sg_expr,
        result_ty,
        ..
    } = instr
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Pattern 4: StructGet { field, LocalGet(cast_alias) } via alias
    if let WirInstr::StructGet {
        field_name,
        expr: sg_expr,
        result_ty,
        ..
    } = instr
        && let WirInstr::LocalGet {
            name: alias_name, ..
        } = sg_expr.as_ref()
        && let Some((temp_name, cast_type_idx)) = refcast_aliases.get(alias_name.as_str())
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (*cast_type_idx, field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Recurse into children
    instr.for_each_boxed_child_mut(&mut |child| {
        replace_variant_accesses(child, variant_replacements, refcast_aliases);
    });
}

/// Check if instruction is `LocalSet { name, value: <wrapper>(Call { func_id in candidates }) }`.
fn is_candidate_call_set(
    instr: &WirInstr,
    expected_name: &str,
    candidate_ids: &IndexSet<u32>,
) -> bool {
    let WirInstr::LocalSet { name, value } = instr else {
        return false;
    };
    if name != expected_name {
        return false;
    }
    unwrap_to_candidate_call(value, candidate_ids).is_some()
}

/// Extract (`func_id_index`, `temp_name`) from a candidate call `LocalSet`,
/// unwrapping trivial `Block` / `Seq` wrappers left by inlining.
fn extract_candidate_call_info(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
) -> Option<(u32, String)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    unwrap_to_candidate_call(value, candidate_ids).map(|idx| (idx, name.clone()))
}

/// Take the Call instruction out of a `LocalSet`, unwrapping trivial
/// `Block` / `Seq` wrappers. Replaces the instruction with Nop. Returns
/// `(prefix_instrs, call_instr)` where prefix instructions are statements
/// from inside the wrappers that must be emitted before the call (e.g.
/// initialization of locals used as call arguments). Consumes the same
/// [`resolve_wrapped_result`] path validation used, so the two cannot
/// disagree on where the call and its prefixes live.
fn take_call_from_local_set(instr: &mut WirInstr) -> (Vec<WirInstr>, Box<WirInstr>) {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let (steps, _) = resolve_wrapped_result(&value)
        .unwrap_or_else(|| unreachable!("SROA call-site take on a shape validation rejected"));
    let mut prefix = Vec::new();
    let mut current = *value;
    for step in steps {
        let (mut list, value_idx) = match (step, current) {
            (
                ResultStep::Seq { value_idx } | ResultStep::BreakValue { value_idx },
                WirInstr::Seq(body),
            )
            | (ResultStep::Block { value_idx }, WirInstr::Block { body, .. }) => (body, value_idx),
            _ => unreachable!("resolve_wrapped_result path mismatch"),
        };
        for item in list.drain(..value_idx) {
            if !matches!(item, WirInstr::Nop) {
                prefix.push(item);
            }
        }
        // The value now sits at index 0; the rest (a break `Br`, a trailing
        // `Unreachable`) is dropped with the wrapper.
        current = list.swap_remove(0);
    }
    match current {
        call @ WirInstr::Call { .. } => (prefix, Box::new(call)),
        _ => unreachable!("expected Call at SROA call-site result leaf"),
    }
}

// Nested variant-slot flattening.
//
// Single-level SROA turns `next_element<i64>` into a function returning the
// multi-value vector `[outer_disc, ref Option<i64>, ref Err]`. A `ref W` result
// slot whose `W` is itself a small variant is still heap-boxed at every return
// (`struct.new W::Case`) and immediately matched at every call site. This phase
// splits such a slot into `W`'s own multi-value layout `[inner_disc,
// payloads...]` (via [`compute_variant_layout`], the same layout engine
// single-level SROA uses), removing the per-element inner allocation — the
// dominant cost when deserializing arrays of scalars.
//
// This generalizes the layout analysis recursively: the inner `W` may be any
// eligible `SubtypeHierarchy` variant (`Result`, a multi-case variant, an
// `Option<(scalar, scalar)>`), not just `Option<scalar>`. Only `Option<&T>`
// null-niche variants (already unboxed, no discriminant field) are out of
// scope. Running the pass to a fix-point flattens nested-in-nested slots one
// level per round.
//
// Bail-everywhere: a slot is expanded only when every return of the function
// decomposes structurally and every call site's slot local is consumed solely
// via variant access — either directly, or through the lowered `?`-unwrap
// `local = if Ok { payload } else { return Err }` (whose then-arm copies the
// slot out and whose else-arm diverges). Any other shape leaves the slot at its
// single-level (boxed) form, which is already correct.

/// A confirmed slot-flatten target: result slot `slot` of function `func_idx`,
/// a `ref W`, expands into `W`'s multi-value layout.
struct SlotFlattenCand {
    func_idx: usize,
    func_id_index: u32,
    /// Result-vector index of the `ref W` slot to expand.
    slot: usize,
    /// WIR type index of the inner variant `W`.
    variant_type_idx: u32,
    /// `W`'s multi-value layout (`[i32 disc, payloads...]`).
    layout: VariantLayout,
}

/// If result-slot type `slot_ty` is a `ref` to an eligible `SubtypeHierarchy`
/// variant, return `(variant_type_idx, layout)`. Null-niche (`NullableRef`)
/// variants — `Option<&T>` and friends, already unboxed with no discriminant
/// field — are out of scope.
fn slot_variant_layout(module: &WirPackage, slot_ty: &WirType) -> Option<(u32, VariantLayout)> {
    let WirType::Ref { type_id, .. } = slot_ty else {
        return None;
    };
    let v_idx = type_id.index();
    let WirTypeDef::Variant(v) = module.types.get(v_idx as usize)? else {
        return None;
    };
    if !matches!(v.repr, WirVariantRepr::SubtypeHierarchy) {
        return None;
    }
    let layout = compute_variant_layout(module, v_idx, v)?;
    Some((v_idx, layout))
}

/// Build the all-default result vector for `cand`'s inner variant — used when a
/// return leaves the slot unused (a null ref, e.g. an `Err` return). The
/// discriminant takes a unit case's value when one exists (semantically
/// "empty"), else 0; the value is never read on that path, since the outer
/// discriminant selects a different case.
fn default_variant_vector(cand: &SlotFlattenCand) -> Vec<WirInstr> {
    let vi = &cand.layout.variant_info;
    let disc = vi
        .case_type_indices
        .iter()
        .position(Option::is_none)
        .and_then(|p| i32::try_from(p).ok())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(cand.layout.field_types.len());
    out.push(WirInstr::I32Const(disc));
    for ty in &cand.layout.field_types[1..] {
        out.push(default_value_for_type(ty));
    }
    out
}

/// True when `type_id` belongs to variant `v_idx`'s family (the base variant
/// type or one of its case structs).
fn is_variant_family(module: &WirPackage, type_id: u32, v_idx: u32) -> bool {
    type_id == v_idx
        || matches!(module.variant_case_info.get(&type_id), Some((p, _)) if *p == v_idx)
}

/// Look through a `RefAsNonNull` narrowing wrapper (single-level SROA emits the
/// `Ok(None)` payload as `RefAsNonNull(StructNew(..))`).
fn peel_ref_as_non_null(expr: &WirInstr) -> &WirInstr {
    match expr {
        WirInstr::RefAsNonNull(inner) => peel_ref_as_non_null(inner),
        other => other,
    }
}

/// A slot field at a multi-value return is decomposable when it is a `StructNew`
/// of the inner variant family (Ok(Some)/Ok(None)) or a null ref (the slot is
/// unused on this return, e.g. an `Err` return), optionally under a
/// `RefAsNonNull` wrapper.
fn slot_field_decomposable(expr: &WirInstr, v_idx: u32, module: &WirPackage) -> bool {
    match peel_ref_as_non_null(expr) {
        WirInstr::StructNew { type_id, fields } => {
            !fields.is_empty() && is_variant_family(module, type_id.index(), v_idx)
        }
        WirInstr::RefNull { .. } => true,
        _ => false,
    }
}

/// Verify every value-returning `Return` reachable in `instr` yields a
/// multi-value `Seq` of `arity` whose `slot` field is decomposable. Walks every
/// child position via `for_each_child` so the validator's coverage matches the
/// rewriter (`rewrite_slot_returns`, which uses `for_each_boxed_child_mut`):
/// a `?` desugar can leave a `Return` inside an `If` condition or a `LocalSet`
/// value, and the rewriter visits those, so the validator must too or it would
/// confirm a function whose un-validated return is later left at the old arity.
fn all_returns_decompose(
    instr: &WirInstr,
    arity: usize,
    slot: usize,
    v_idx: u32,
    module: &WirPackage,
) -> bool {
    if let WirInstr::Return { value: Some(v) } = instr {
        let decomposable = match v.as_ref() {
            WirInstr::Seq(fields) => {
                fields.len() == arity
                    && slot < fields.len()
                    && slot_field_decomposable(&fields[slot], v_idx, module)
            }
            _ => false,
        };
        if !decomposable {
            return false;
        }
    }
    let mut ok = true;
    instr.for_each_child(&mut |c| {
        if ok && !all_returns_decompose(c, arity, slot, v_idx, module) {
            ok = false;
        }
    });
    ok
}

/// Phase 1: collect functions with an expandable `ref W` result slot whose
/// returns all decompose. One slot per function per round; the pass's
/// fix-point loop revisits functions to reach deeper slots.
fn slot_flatten_candidates(module: &WirPackage, pinned: &IndexSet<u32>) -> Vec<SlotFlattenCand> {
    let mut out = Vec::new();
    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = module.defined_func_base + u32::try_from(i).unwrap();
        if pinned.contains(&func_id_index) || func.body.is_none() {
            continue;
        }
        let Some(WirTypeDef::Func(ft)) = module.types.get(func.type_id.index() as usize) else {
            continue;
        };
        // Only already-multi-value functions have a `ref W` *slot* to flatten;
        // sole-result `ref W` returns are the province of single-level SROA.
        if ft.results.len() < 2 {
            continue;
        }
        let arity = ft.results.len();
        for slot in 0..arity {
            let Some((v_idx, layout)) = slot_variant_layout(module, &ft.results[slot]) else {
                continue;
            };
            // Splicing the slot into `W`'s layout changes the result arity by
            // `layout.len() - 1`; keep it within the per-case cap.
            let new_arity = arity - 1 + layout.field_types.len();
            if new_arity > MAX_PER_CASE_RESULT_FIELDS {
                continue;
            }
            let body = func.body.as_ref().unwrap();
            if !body
                .iter()
                .all(|instr| all_returns_decompose(instr, arity, slot, v_idx, module))
            {
                continue;
            }
            out.push(SlotFlattenCand {
                func_idx: i,
                func_id_index,
                slot,
                variant_type_idx: v_idx,
                layout,
            });
            break; // one slot per function per round
        }
    }
    out
}

/// Classification of how a call site consumes the slot local.
enum SlotConsumer {
    /// `ref.test`/`ref.cast` directly on the slot local.
    Direct,
    /// The lowered `?`-unwrap: `LocalSet(alias, If { Ok => slot, else => diverge })`.
    /// `alias` carries the unboxed Option onward and is matched downstream.
    Unwrap { alias: String },
}

/// True when a `?`-unwrap then-arm is *exactly* the slot copied out — either
/// `[<slot extraction>]` or `[LocalSet(t, <slot extraction>), LocalGet(t)]`,
/// where `<slot extraction>` is `LocalGet(slot)` under zero or more
/// `RefAsNonNull` wrappers. `rewrite_unwrap_to_guard` discards the whole
/// then-arm, so anything richer (a side-effecting prefix statement, a branch, a
/// second definition of the temp) must be rejected or a needed statement would
/// be silently dropped. For the two-statement form the copy temp `t` must
/// have no reads outside the discarded pair (`def_use.get_count(t) == 1` —
/// the pair's own `LocalGet`): a read elsewhere would observe a local whose
/// only definition was dropped.
fn then_is_pure_slot_copy(then_body: &[WirInstr], slot_local: &str, def_use: &LocalDefUse) -> bool {
    fn is_slot_extraction(e: &WirInstr, slot: &str) -> bool {
        match e {
            WirInstr::RefAsNonNull(inner) => is_slot_extraction(inner, slot),
            WirInstr::LocalGet { name, .. } => name == slot,
            _ => false,
        }
    }
    match then_body {
        [single] => is_slot_extraction(single, slot_local),
        [
            WirInstr::LocalSet { name: t, value },
            WirInstr::LocalGet { name: g, .. },
        ] => {
            g == t
                && is_slot_extraction(value, slot_local)
                && def_use.get_count(t) == 1
                && def_use.set_count(t) == 1
        }
        _ => false,
    }
}

/// Find the unique `?`-unwrap `LocalSet(alias, If {...})` whose then-arm copies
/// `slot_local` out and whose else-arm diverges. Returns the alias name.
fn find_unwrap_alias(body: &[WirInstr], slot_local: &str, def_use: &LocalDefUse) -> Option<String> {
    let mut found = None;
    fn walk(
        body: &[WirInstr],
        slot_local: &str,
        def_use: &LocalDefUse,
        found: &mut Option<String>,
    ) {
        for instr in body {
            if let WirInstr::LocalSet { name, value } = instr
                && let WirInstr::If {
                    then_body,
                    else_body: Some(eb),
                    ..
                } = value.as_ref()
                && eb.iter().any(WirInstr::always_diverges)
                && then_is_pure_slot_copy(then_body, slot_local, def_use)
            {
                *found = Some(name.clone());
            }
            match instr {
                WirInstr::Block { body, .. }
                | WirInstr::Loop { body, .. }
                | WirInstr::Seq(body) => {
                    walk(body, slot_local, def_use, found);
                }
                WirInstr::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    walk(then_body, slot_local, def_use, found);
                    if let Some(eb) = else_body {
                        walk(eb, slot_local, def_use, found);
                    }
                }
                _ => {}
            }
        }
    }
    walk(body, slot_local, def_use, &mut found);
    found
}

/// Classify the consumer of `slot_local` in `body`, or `None` if not cleanly
/// rewritable. `case_types` is the inner variant's payload-bearing case set
/// (the only type the nested rewrite's `VariantReplacement` maps carry).
fn classify_slot_consumer(
    body: &[WirInstr],
    slot_local: &str,
    case_types: &IndexSet<u32>,
    def_use: &LocalDefUse,
) -> Option<SlotConsumer> {
    if all_uses_are_variant_access(body, slot_local, case_types, def_use) {
        return Some(SlotConsumer::Direct);
    }
    // Otherwise the only use must be a single `?`-unwrap copy whose alias is
    // itself consumed only via variant access.
    if def_use.get_count(slot_local) != 1 {
        return None;
    }
    let alias = find_unwrap_alias(body, slot_local, def_use)?;
    if def_use.set_count(&alias) == 1
        && all_uses_are_variant_access(body, &alias, case_types, def_use)
    {
        Some(SlotConsumer::Unwrap { alias })
    } else {
        None
    }
}

/// The payload-bearing case struct type indices of a candidate's inner variant
/// — the only ids the call-site rewriter's `VariantReplacement` maps carry,
/// hence the only ones a `RefTest` / `RefCast` on the slot local may name.
fn slot_case_types(cand: &SlotFlattenCand) -> IndexSet<u32> {
    cand.layout
        .variant_info
        .case_type_indices
        .iter()
        .flatten()
        .copied()
        .collect()
}

/// Phase 2: keep candidates whose every call site consumes the slot cleanly.
fn validate_slot_sites(module: &WirPackage, cands: Vec<SlotFlattenCand>) -> Vec<SlotFlattenCand> {
    let def_use_by_func: Vec<LocalDefUse> = module
        .functions
        .iter()
        .map(|func| {
            func.body
                .as_deref()
                .map(LocalDefUse::of_body)
                .unwrap_or_default()
        })
        .collect();
    cands
        .into_iter()
        .filter(|cand| {
            let case_types = slot_case_types(cand);
            let mut saw_call = false;
            let mut all_ok = true;
            for (i, func) in module.functions.iter().enumerate() {
                let Some(body) = &func.body else { continue };
                let def_use = &def_use_by_func[i];
                // Every reference to the candidate must be a multi-value bind we
                // can rewrite. A `Return(Call(f))` tail call (or any other raw
                // call) would keep the old arity after we widen the signature,
                // producing a Wasm type mismatch — bail if one exists.
                let total_calls: usize = body
                    .iter()
                    .map(|i| count_calls_to(i, cand.func_id_index))
                    .sum();
                let mut mvbind_calls = 0usize;
                for_each_multivalue_call(body, cand.func_id_index, &mut |locals| {
                    saw_call = true;
                    mvbind_calls += 1;
                    match locals.get(cand.slot).and_then(|o| o.as_ref()) {
                        Some(slot_local) => {
                            if classify_slot_consumer(body, slot_local, &case_types, def_use)
                                .is_none()
                            {
                                all_ok = false;
                            }
                        }
                        None => all_ok = false, // slot dropped: nothing to gain, bail
                    }
                });
                if total_calls != mvbind_calls {
                    all_ok = false;
                }
            }
            saw_call && all_ok
        })
        .collect()
}

/// Count every `Call` to `func_id_index` anywhere in `body` (all positions).
fn count_calls_to(instr: &WirInstr, func_id_index: u32) -> usize {
    let mut n = 0;
    if let WirInstr::Call { func_id, .. } = instr
        && func_id.index() == func_id_index
    {
        n += 1;
    }
    instr.for_each_child(&mut |c| n += count_calls_to(c, func_id_index));
    n
}

/// Visit each `MultiValueLocalBind { Call(func_id_index), locals }` in a body.
///
/// Deliberately recurses only into statement-list bodies (Block/Loop/Seq, If
/// arms) — NOT into If conditions / `LocalSet` values — so it stays symmetric
/// with `expand_slot_binds` (which rewrites the same positions). A call in a
/// non-statement position is counted by `count_calls_to` (which visits every
/// position via `for_each_child`) but not here, so `total_calls != mvbind_calls`
/// bails the candidate. Do not switch this to `for_each_child` without also
/// teaching `expand_slot_binds` to rewrite those positions.
fn for_each_multivalue_call(
    body: &[WirInstr],
    func_id_index: u32,
    f: &mut impl FnMut(&[Option<String>]),
) {
    for instr in body {
        if let WirInstr::MultiValueLocalBind {
            instr: call,
            locals,
        } = instr
            && let Some(WirInstr::Call { func_id, .. }) = unwrap_to_inner_call(call)
            && func_id.index() == func_id_index
        {
            f(locals);
        }
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                for_each_multivalue_call(body, func_id_index, f);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                for_each_multivalue_call(then_body, func_id_index, f);
                if let Some(eb) = else_body {
                    for_each_multivalue_call(eb, func_id_index, f);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn flatten_variant_slots(module: &mut WirPackage) {
    let pinned = collect_pinned_func_ids(module);
    // Fix-point: each round flattens at most one `ref W` slot per function.
    // A round that flattens `[.., ref Result<ref Option<i64>, E>, ..]` exposes
    // the inner `ref Option<i64>` for the next round, so nested-in-nested slots
    // peel one level at a time. Bounded by the arity cap: every flatten adds at
    // least one field, and no result vector exceeds `MAX_PER_CASE_RESULT_FIELDS`.
    loop {
        let cands = slot_flatten_candidates(module, &pinned);
        if cands.is_empty() {
            break;
        }
        let confirmed = validate_slot_sites(module, cands);
        compiler_trace!(
            "sroa_variant_return",
            "slot-flatten confirmed = {}",
            confirmed.len()
        );
        if confirmed.is_empty() {
            break;
        }
        apply_slot_flatten(module, &confirmed);
    }
}

/// Phase 3: rewrite signatures, returns, and call sites of confirmed slot
/// candidates.
fn apply_slot_flatten(module: &mut WirPackage, confirmed: &[SlotFlattenCand]) {
    let by_func: crate::hashmap::IndexMap<u32, &SlotFlattenCand> =
        confirmed.iter().map(|c| (c.func_id_index, c)).collect();

    // Step A: signatures + returns of the confirmed functions.
    for cand in confirmed {
        let func = &mut module.functions[cand.func_idx];
        let old_type_idx = func.type_id.index() as usize;
        let WirTypeDef::Func(ft) = &module.types[old_type_idx] else {
            unreachable!()
        };
        let old_arity = ft.results.len();
        let mut results = ft.results.clone();
        results.splice(
            cand.slot..=cand.slot,
            cand.layout.field_types.iter().cloned(),
        );
        let new_ft = WirFuncType {
            name: ft.name.clone(),
            params: ft.params.clone(),
            results,
        };
        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_ft));
        let func = &mut module.functions[cand.func_idx];
        func.type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                rewrite_slot_returns(instr, cand, old_arity);
            }
        }
    }

    // Step B: call sites in all bodies.
    for i in 0..module.functions.len() {
        if module.functions[i].body.is_some() {
            let mut body = module.functions[i].body.take().unwrap();
            rewrite_slot_call_sites(&mut body, &by_func, &module.types);
            module.functions[i].body = Some(body);
        }
    }
}

/// Decompose a slot value — a `StructNew` of the inner variant `W` (optionally
/// under `RefAsNonNull`), or a null ref — into `W`'s flat result vector `[disc,
/// payloads...]`, reusing the same [`pad_variant_fields`] machinery that
/// single-level SROA uses at return sites.
fn decompose_slot_to_vector(slot_expr: WirInstr, cand: &SlotFlattenCand) -> Vec<WirInstr> {
    let mut expr = slot_expr;
    while let WirInstr::RefAsNonNull(inner) = expr {
        expr = *inner;
    }
    match expr {
        WirInstr::StructNew { type_id, fields } if !fields.is_empty() => pad_variant_fields(
            fields,
            &cand.layout.variant_info,
            &cand.layout.field_types,
            type_id.index(),
        ),
        // Null / unused slot: all-default `W` vector (never read on this path).
        _ => default_variant_vector(cand),
    }
}

/// Split the slot field of every multi-value `Return` `Seq` of the function
/// into `W`'s flat layout. Walks every child position — a `?` desugar can
/// leave a `Return` inside an `If` condition or a `LocalSet` value, not just in
/// a block body — and only touches `Return`s whose `Seq` has the pre-widening
/// arity and whose slot field is the inner-variant value (a `StructNew`/null),
/// so an unrelated same-arity return is left alone.
fn rewrite_slot_returns(instr: &mut WirInstr, cand: &SlotFlattenCand, old_arity: usize) {
    if let WirInstr::Return { value: Some(v) } = instr
        && let WirInstr::Seq(fields) = v.as_mut()
        && fields.len() == old_arity
        && cand.slot < fields.len()
        && matches!(
            peel_ref_as_non_null(&fields[cand.slot]),
            WirInstr::StructNew { .. } | WirInstr::RefNull { .. }
        )
    {
        let slot_expr = std::mem::replace(&mut fields[cand.slot], WirInstr::Nop);
        let expanded = decompose_slot_to_vector(slot_expr, cand);
        fields.splice(cand.slot..=cand.slot, expanded);
    }
    instr.for_each_boxed_child_mut(&mut |c| rewrite_slot_returns(c, cand, old_arity));
}

/// Rewrite all slot-candidate call sites in one body. Classification runs in a
/// read-only plan pass over the *whole* body (same scope `validate_slot_sites`
/// uses), so the consumer verdict never depends on where the bind sits or on
/// mutations a prior bind made; the mutation pass then consumes those plans.
fn rewrite_slot_call_sites(
    body: &mut Vec<WirInstr>,
    by_func: &crate::hashmap::IndexMap<u32, &SlotFlattenCand>,
    types: &[WirTypeDef],
) {
    // Plan: classify every candidate call site's slot consumer against the full
    // body. Keyed by slot local (single-level SROA gives each call site a unique
    // slot-local name).
    let mut plans: crate::hashmap::IndexMap<String, SlotConsumer> =
        crate::hashmap::IndexMap::default();
    {
        let root: &[WirInstr] = body;
        let def_use = LocalDefUse::of_body(root);
        for instr in root {
            plan_slot_call_sites(instr, root, by_func, &def_use, &mut plans);
        }
    }
    if plans.is_empty() {
        return;
    }
    // Turn each `?`-unwrap binding into a guard, dropping the (pure) slot copy.
    for consumer in plans.values() {
        if let SlotConsumer::Unwrap { alias } = consumer {
            rewrite_unwrap_to_guard(body, alias);
        }
    }
    // Expand the binds and register the inner-variant replacements.
    let mut replacements: crate::hashmap::IndexMap<String, VariantReplacement> =
        crate::hashmap::IndexMap::default();
    expand_slot_binds(body, by_func, &plans, types, &mut replacements);
    // Replace variant accesses on the slot local / unwrap alias.
    let mut aliases: crate::hashmap::IndexMap<String, (String, u32)> =
        crate::hashmap::IndexMap::default();
    collect_refcast_aliases(body, &replacements, &mut aliases);
    for instr in body.iter_mut() {
        replace_variant_accesses(instr, &replacements, &aliases);
    }
}

/// Read-only plan pass: record `slot_local -> SlotConsumer` for every candidate
/// `MultiValueLocalBind`, classifying against `root` (the full function body).
fn plan_slot_call_sites(
    instr: &WirInstr,
    root: &[WirInstr],
    by_func: &crate::hashmap::IndexMap<u32, &SlotFlattenCand>,
    def_use: &LocalDefUse,
    plans: &mut crate::hashmap::IndexMap<String, SlotConsumer>,
) {
    if let WirInstr::MultiValueLocalBind {
        instr: call,
        locals,
    } = instr
        && let Some(WirInstr::Call { func_id, .. }) = unwrap_to_inner_call(call)
        && let Some(cand) = by_func.get(&func_id.index())
        && let Some(Some(slot_local)) = locals.get(cand.slot)
    {
        let case_types = slot_case_types(cand);
        if let Some(consumer) = classify_slot_consumer(root, slot_local, &case_types, def_use) {
            plans.insert(slot_local.clone(), consumer);
        }
    }
    instr.for_each_child(&mut |c| plan_slot_call_sites(c, root, by_func, def_use, plans));
}

/// Mutation pass: at each candidate bind, split the slot local into `W`'s
/// result-vector locals, declare them, and register the inner
/// `VariantReplacement` (built by [`build_variant_replacement`], the shared
/// call-site machinery) keyed by the `?`-unwrap alias (the unwrap was already
/// turned into a guard) or, for a direct consumer, by the slot local itself.
fn expand_slot_binds(
    body: &mut Vec<WirInstr>,
    by_func: &crate::hashmap::IndexMap<u32, &SlotFlattenCand>,
    plans: &crate::hashmap::IndexMap<String, SlotConsumer>,
    types: &[WirTypeDef],
    replacements: &mut crate::hashmap::IndexMap<String, VariantReplacement>,
) {
    let mut i = 0;
    while i < body.len() {
        // Post-order: recurse into nested bodies before rewriting this level.
        match &mut body[i] {
            WirInstr::Block { body: b, .. } | WirInstr::Loop { body: b, .. } | WirInstr::Seq(b) => {
                expand_slot_binds(b, by_func, plans, types, replacements);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                expand_slot_binds(then_body, by_func, plans, types, replacements);
                if let Some(eb) = else_body {
                    expand_slot_binds(eb, by_func, plans, types, replacements);
                }
            }
            _ => {}
        }

        let cand_slot = match &body[i] {
            WirInstr::MultiValueLocalBind {
                instr: call,
                locals,
            } => unwrap_to_inner_call(call)
                .and_then(|c| match c {
                    WirInstr::Call { func_id, .. } => by_func.get(&func_id.index()),
                    _ => None,
                })
                .map(|c| (*c, locals.clone())),
            _ => None,
        };
        let Some((cand, locals)) = cand_slot else {
            i += 1;
            continue;
        };
        let Some(Some(slot_local)) = locals.get(cand.slot).cloned() else {
            i += 1;
            continue;
        };
        // `validate_slot_sites` confirmed this site, so the plan pass (same
        // classification, same scope) must have recorded a consumer.
        let consumer = plans.get(&slot_local).unwrap_or_else(|| {
            unreachable!("slot-flatten: confirmed call site `{slot_local}` has no plan")
        });

        // Fresh locals for `W`'s result-vector fields, mapped by field name so
        // `build_variant_replacement` can resolve each case struct's fields. The
        // field name (`discriminant`, `payload_0`, `caseN_payload_M`) is kept in
        // the local name so the flattened slot stays self-documenting.
        let mut field_map: crate::hashmap::IndexMap<String, String> =
            crate::hashmap::IndexMap::default();
        let mut sub_locals: Vec<Option<String>> = Vec::with_capacity(cand.layout.field_names.len());
        let mut decls: Vec<WirInstr> = Vec::with_capacity(cand.layout.field_names.len());
        for (field_name, field_ty) in cand.layout.field_names.iter().zip(&cand.layout.field_types) {
            let name = format!("{slot_local}__n_{field_name}");
            field_map.insert(field_name.clone(), name.clone());
            decls.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty: field_ty.clone(),
            });
            sub_locals.push(Some(name));
        }

        // Expand the bind's locals: replace the single slot local with `W`'s
        // result-vector locals.
        if let WirInstr::MultiValueLocalBind { locals, .. } = &mut body[i] {
            locals.splice(cand.slot..=cand.slot, sub_locals);
        }

        let ivr = build_variant_replacement(
            &field_map,
            &cand.layout.variant_info,
            cand.variant_type_idx,
            types,
        );

        let key = match consumer {
            SlotConsumer::Direct => slot_local,
            SlotConsumer::Unwrap { alias } => alias.clone(),
        };
        replacements.insert(key, ivr);

        let decls_len = decls.len();
        body.splice(i..i, decls);
        i += decls_len + 1; // decls + the bind
    }
}

/// Replace `LocalSet(alias, If { cond, then, else })` (the `?`-unwrap) with the
/// guard `If { cond, then: [], else }`, dropping the slot copy.
fn rewrite_unwrap_to_guard(body: &mut [WirInstr], alias: &str) {
    for instr in body.iter_mut() {
        if let WirInstr::LocalSet { name, value } = instr
            && name == alias
            && matches!(
                value.as_ref(),
                WirInstr::If {
                    else_body: Some(_),
                    ..
                }
            )
        {
            if let WirInstr::If {
                condition,
                else_body,
                ..
            } = value.as_mut()
            {
                let cond = std::mem::replace(condition.as_mut(), WirInstr::Nop);
                let eb = else_body.take();
                *instr = WirInstr::If {
                    condition: Box::new(cond),
                    result: None,
                    then_body: Vec::new(),
                    else_body: eb,
                };
            }
            continue;
        }
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                rewrite_unwrap_to_guard(body, alias);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                rewrite_unwrap_to_guard(then_body, alias);
                if let Some(eb) = else_body {
                    rewrite_unwrap_to_guard(eb, alias);
                }
            }
            _ => {}
        }
    }
}
