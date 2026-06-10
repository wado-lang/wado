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
    WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId, WirVariantType,
};

use super::util::collect_pinned_func_ids;

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
struct VariantSroaInfo {
    /// WIR type indices of the case struct types (index = case discriminant).
    case_type_indices: Vec<Option<u32>>,
    /// Number of payload fields per case.
    case_payload_counts: Vec<usize>,
    /// Maximum payload count across all cases.
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
/// Primitive scalars (i32, i64, f32, f64) are always eligible.
/// Concrete GC refs (`ref $T`, `ref null $T`) are also eligible: Wasm multi-value
/// returns support any value type, including GC refs. This allows SROA of structs
/// with GC ref fields, such as tuples containing String values.
/// Abstract heap refs (`ref null struct`, etc.) are excluded as they lack
/// the precise type information needed for `StructGet` replacement.
pub(super) fn is_eligible_field_type(ty: &WirType) -> bool {
    !matches!(ty, WirType::AbstractRef { .. } | WirType::Unit)
}

/// Structural equality for `WirType` (not derived because `WirTypeId` has no `PartialEq`).
fn wir_types_equal(a: &WirType, b: &WirType) -> bool {
    match (a, b) {
        (WirType::I8, WirType::I8)
        | (WirType::I16, WirType::I16)
        | (WirType::I32, WirType::I32)
        | (WirType::I64, WirType::I64)
        | (WirType::U8, WirType::U8)
        | (WirType::U16, WirType::U16)
        | (WirType::U32, WirType::U32)
        | (WirType::U64, WirType::U64)
        | (WirType::F32, WirType::F32)
        | (WirType::F64, WirType::F64)
        | (WirType::Bool, WirType::Bool)
        | (WirType::Char, WirType::Char)
        | (WirType::Unit, WirType::Unit) => true,
        (
            WirType::Ref {
                type_id: a_id,
                nullable: a_null,
            },
            WirType::Ref {
                type_id: b_id,
                nullable: b_null,
            },
        ) => a_id.index() == b_id.index() && a_null == b_null,
        (WirType::Enum { type_id: a_id }, WirType::Enum { type_id: b_id })
        | (WirType::Flags { type_id: a_id }, WirType::Flags { type_id: b_id }) => {
            a_id.index() == b_id.index()
        }
        _ => false,
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
fn reads_only_local_state(expr: &WirInstr) -> bool {
    match expr {
        // Loads from heap / memory / GC state.
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
        | WirInstr::MemorySize => false,
        // Calls — observable + might depend on intervening state.
        WirInstr::Call { .. }
        | WirInstr::CallIndirect { .. }
        | WirInstr::CallRef { .. } => false,
        // Stores — observable in the moving-past-intervening sense too:
        // a relocated store would mutate state at a different program
        // point. Reject conservatively.
        WirInstr::GlobalSet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. }
        | WirInstr::ArrayCopy { .. }
        | WirInstr::ArrayFill { .. }
        | WirInstr::TableSet { .. }
        | WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. }
        | WirInstr::V128Store { .. }
        | WirInstr::MemoryGrow(_)
        | WirInstr::MemoryFill { .. } => false,
        // Control-flow exits embedded in a value would be bizarre — reject.
        WirInstr::Return { .. }
        | WirInstr::Br { .. }
        | WirInstr::BrIf { .. }
        | WirInstr::BrTable { .. }
        | WirInstr::Unreachable => false,
        // Anything else (constants, arithmetic, ref ops, `StructNew` and
        // `ArrayNew*` allocations, `RefAsNonNull`, `RefCast`, `LocalGet`
        // / `LocalSet` / `LocalTee` — the last three are tracked
        // explicitly by `collect_local_io`) is fine to relocate provided
        // its children are.
        _ => {
            let mut ok = true;
            expr.for_each_child(&mut |child| {
                if ok && !reads_only_local_state(child) {
                    ok = false;
                }
            });
            ok
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
    for (i, func) in module.functions.iter_mut().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
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

/// Maximum distance the relocation peephole will look ahead from a
/// `LocalSet` to find a matching `Return(LocalGet)`. Pragmatic upper bound;
/// HFS-emitted patterns put the write-back stmt right before the `Return`.
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

    let end = (start_idx + 1 + RETURN_TEMP_INTERVENING_BUDGET).min(instrs.len());
    for j in (start_idx + 1)..end {
        if let WirInstr::Return { value: Some(rv) } = &instrs[j]
            && let WirInstr::LocalGet { name: rn, .. } = rv.as_ref()
            && rn == name
        {
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
///    `Return { Some(Call(c)) }` where `c` is already accepted *and* shares
///    the same variant return type. The seed round runs with an empty
///    "already accepted" set, so it only accepts the leaf functions whose
///    returns are direct `StructNew`s. Each subsequent round can then pick
///    up callers whose tail calls now target accepted callees.
fn find_sroa_candidates(module: &WirPackage, pinned: &IndexSet<u32>) -> Vec<(u32, SroaCandidate)> {
    // Stage 1: collect potential candidates with layout info.
    let mut potentials: Vec<PotentialCandidate> = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();

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

    // Stage 2: fix-point. A function is accepted when its return shapes are
    // all StructNew/Unreachable, optionally with `Return { Some(Call(c)) }`
    // tail-calls to already-accepted candidates with the same variant type.
    let mut accepted: IndexSet<u32> = IndexSet::default();
    loop {
        // Group accepted candidates by variant_type_idx so the tail-call
        // check can constrain matches to functions returning the same
        // variant. Same variant -> same multi-value sig, so swapping
        // ABI is sound.
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

        let mut changed = false;
        for p in &potentials {
            if accepted.contains(&p.func_id_index) {
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
            if all_returns_are_variant_struct_new(body, &p.valid_case_type_indices, tail_call_set) {
                accepted.insert(p.func_id_index);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    potentials
        .into_iter()
        .filter(|p| accepted.contains(&p.func_id_index))
        .map(|p| (p.func_id_index, p.candidate))
        .collect()
}

/// Compute the SROA layout info for a variant-returning function.
///
/// Returns `(SroaCandidate, valid_case_type_indices)` when the variant is
/// representable as a small multi-value tuple. The caller separately verifies
/// that every `Return` in the body is a leaf shape compatible with this
/// layout (`all_returns_are_variant_struct_new`), so this stage can be
/// re-used across the fix-point's rounds without touching the body.
///
/// A variant is eligible (layout-wise) if:
/// - All payload types across all cases are eligible scalar types
/// - Max payload count across all cases is ≤ 3 (so total fields ≤ 4: disc + 3 payloads)
/// - Case type indices can be resolved via `variant_case_info`
fn analyze_variant_layout(
    module: &WirPackage,
    func_array_idx: usize,
    variant_type_idx: u32,
    variant_type: &WirVariantType,
) -> Option<(SroaCandidate, IndexSet<u32>)> {
    // Collect per-case info: case type index and payload count
    let mut case_type_indices: Vec<Option<u32>> = Vec::with_capacity(variant_type.cases.len());
    let mut case_payload_counts: Vec<usize> = Vec::with_capacity(variant_type.cases.len());
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
        case_payload_counts.push(payload_count);
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
    if field_count > 4 {
        return None;
    }

    // Compute the payload types: try shared layout first, fall back to per-case slots.
    let mut homogeneous = true;
    for pos in 0..max_payload_count {
        let mut found: Option<&WirType> = None;
        for case in &variant_type.cases {
            if let Some(ty) = case.payload.get(pos) {
                if let Some(existing) = found {
                    if !wir_types_equal(existing, ty) {
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
        if fc > 8 {
            return None; // too many multi-value returns
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

    Some((
        SroaCandidate {
            func_array_idx,
            struct_type_idx: variant_type_idx,
            valid_case_type_indices: all_case_type_indices.clone(),
            field_types,
            field_count,
            field_names,
            variant_info: VariantSroaInfo {
                case_type_indices,
                case_payload_counts,
                max_payload_count: total_payload_slots,
                case_slot_offsets,
            },
        },
        all_case_type_indices,
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
        _ => true,
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
/// (`find_candidate_calls_in_block_prefix` → `validate_call_sites_in_body`)
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
            // Recurse into nested blocks and ifs (both add a depth level)
            match &instrs[i] {
                WirInstr::Block { body, .. } => {
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
                _ => {}
            }
            i += 1;
        }
    }
    true
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
            let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
            let caller_is_candidate = candidate_ids.contains(&func_id_index);
            if let Some(body) = &func.body {
                validate_call_sites_in_body(
                    body,
                    body,
                    &candidate_ids,
                    &mut round_invalid,
                    caller_is_candidate,
                );
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
        .map(|(id, c)| {
            (
                *id,
                SroaCandidate {
                    func_array_idx: c.func_array_idx,
                    struct_type_idx: c.struct_type_idx,
                    valid_case_type_indices: c.valid_case_type_indices.clone(),
                    field_types: c.field_types.clone(),
                    field_count: c.field_count,
                    field_names: c.field_names.clone(),
                    variant_info: VariantSroaInfo {
                        case_type_indices: c.variant_info.case_type_indices.clone(),
                        case_payload_counts: c.variant_info.case_payload_counts.clone(),
                        max_payload_count: c.variant_info.max_payload_count,
                        case_slot_offsets: c.variant_info.case_slot_offsets.clone(),
                    },
                },
            )
        })
        .collect()
}

/// Validate call sites of candidate functions within a flat instruction list.
///
/// `root_body` is the top-level function body — used when checking that the temp local
/// is only accessed via valid patterns across all scopes, not just the current scope.
/// This prevents SROA when a call site is inside a nested block (If/Block) but the temp
/// local is used in the outer scope in a non-StructGet context (e.g. `return temp`).
///
/// `caller_is_candidate` is true when the function being validated is itself
/// a candidate. Only then does `Return { Some(Call(candidate)) }` count as a
/// valid call site shape: the caller's signature is being rewritten to
/// multi-value in the same pass, so the callee's multi-value results
/// propagate through naturally. Inside a non-candidate caller, the same
/// shape would mismatch the (still single-value) caller signature.
fn validate_call_sites_in_body(
    instrs: &[WirInstr],
    root_body: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
    caller_is_candidate: bool,
) {
    for instr in instrs {
        // Recurse into nested statement-level blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(
                    body,
                    root_body,
                    candidate_ids,
                    invalid,
                    caller_is_candidate,
                );
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                // Check condition expression for invalid calls (not in nested block scope)
                find_nested_candidate_calls(condition, candidate_ids, invalid);
                validate_call_sites_in_body(
                    then_body,
                    root_body,
                    candidate_ids,
                    invalid,
                    caller_is_candidate,
                );
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(
                        eb,
                        root_body,
                        candidate_ids,
                        invalid,
                        caller_is_candidate,
                    );
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(
                    body,
                    root_body,
                    candidate_ids,
                    invalid,
                    caller_is_candidate,
                );
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
                if unwrap_to_candidate_call(v, candidate_ids).is_none()
                    && matches!(
                        v.as_ref(),
                        WirInstr::Seq(_) | WirInstr::Block { .. } | WirInstr::If { .. }
                    ) =>
            {
                match v.as_ref() {
                    WirInstr::Seq(body) | WirInstr::Block { body, .. } => {
                        validate_call_sites_in_body(
                            body,
                            root_body,
                            candidate_ids,
                            invalid,
                            caller_is_candidate,
                        );
                    }
                    WirInstr::If {
                        condition,
                        then_body,
                        else_body,
                        ..
                    } => {
                        // Check the condition for any nested candidate calls
                        // (a Call inside the condition isn't a tail call
                        // — it gets invalidated normally), then recurse
                        // into both arms as statement lists.
                        find_nested_candidate_calls(condition, candidate_ids, invalid);
                        validate_call_sites_in_body(
                            then_body,
                            root_body,
                            candidate_ids,
                            invalid,
                            caller_is_candidate,
                        );
                        if let Some(eb) = else_body {
                            validate_call_sites_in_body(
                                eb,
                                root_body,
                                candidate_ids,
                                invalid,
                                caller_is_candidate,
                            );
                        }
                    }
                    _ => unreachable!(),
                }
            }
            // For non-block instructions, check for invalid call uses at this level
            _ => {
                check_invalid_call_uses(
                    instr,
                    root_body,
                    candidate_ids,
                    invalid,
                    caller_is_candidate,
                );
            }
        }
    }

    // Check that LocalSet(Call(candidate)) temps are only used via valid
    // variant-access patterns: RefTest(LocalGet(temp)) or
    // StructGet(RefCast(LocalGet(temp))). Use root_body (the full function
    // body) to catch uses of the temp local in outer scopes.
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let Some(func_id_idx) = unwrap_to_candidate_call(value, candidate_ids)
        {
            // Reject when the local has more than one definition: SROA assumes
            // the temp is exclusively defined by this call. With mutable locals
            // (e.g. `let mut s: String;` assigned in multiple branches), the
            // other definitions would be silently dropped, producing wrong code.
            if count_local_set_in_body(root_body, name) > 1 {
                invalid.insert(func_id_idx);
                continue;
            }
            if !all_uses_are_variant_access(root_body, name) {
                invalid.insert(func_id_idx);
            }
        }
    }
}

/// Count `LocalSet { name, .. }` and `LocalTee { name, .. }` for `local_name`
/// across the entire instruction tree.
fn count_local_set_in_body(instrs: &[WirInstr], local_name: &str) -> usize {
    let mut total = 0;
    for instr in instrs {
        total += count_local_set_in_instr(instr, local_name);
    }
    total
}

fn count_local_set_in_instr(instr: &WirInstr, local_name: &str) -> usize {
    let mut count = match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } if name == local_name => {
            1
        }
        _ => 0,
    };
    instr.for_each_child(&mut |child| {
        count += count_local_set_in_instr(child, local_name);
    });
    count
}

/// Check that every reference to `local_name` is a valid variant access pattern:
/// - `RefTest { expr: LocalGet(name) }` — discriminant test
/// - `StructGet { expr: RefCast { expr: LocalGet(name) } }` — payload access
fn all_uses_are_variant_access(instrs: &[WirInstr], local_name: &str) -> bool {
    for instr in instrs {
        if !check_uses_are_variant_access(instr, local_name, VariantAccessCtx::None) {
            return false;
        }
    }
    true
}

/// Context for checking variant access patterns.
#[derive(Clone, Copy)]
enum VariantAccessCtx {
    /// Not inside any variant access pattern.
    None,
    /// Inside `RefTest` or `RefCast` — `LocalGet` is valid here.
    InsideRefTestOrCast,
}

fn check_uses_are_variant_access(
    instr: &WirInstr,
    local_name: &str,
    ctx: VariantAccessCtx,
) -> bool {
    match instr {
        WirInstr::LocalGet { name, .. } if name == local_name => {
            matches!(ctx, VariantAccessCtx::InsideRefTestOrCast)
        }
        WirInstr::LocalSet { name, value } if name == local_name => {
            // The original assignment — check value subtree
            check_variant_uses_in_subtree(value, local_name)
        }
        WirInstr::LocalTee { name, .. } if name == local_name => false,
        WirInstr::RefTest { expr, .. } | WirInstr::RefCast { expr, .. } => {
            check_uses_are_variant_access(expr, local_name, VariantAccessCtx::InsideRefTestOrCast)
        }
        WirInstr::StructGet { expr, .. } => {
            // StructGet can wrap RefCast which wraps LocalGet — check the chain
            check_uses_are_variant_access(expr, local_name, ctx)
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            for child in body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                    return false;
                }
            }
            true
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            if !check_uses_are_variant_access(condition, local_name, VariantAccessCtx::None) {
                return false;
            }
            for child in then_body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                    return false;
                }
            }
            if let Some(eb) = else_body {
                for child in eb {
                    if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                        return false;
                    }
                }
            }
            true
        }
        WirInstr::Seq(body) => {
            for child in body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                    return false;
                }
            }
            true
        }
        _ => check_variant_uses_in_subtree(instr, local_name),
    }
}

fn check_variant_uses_in_subtree(instr: &WirInstr, local_name: &str) -> bool {
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
            ok = false;
        }
    });
    ok
}

/// Look through `ValueCopy`, trivial `Block` wrappers, and other transparent
/// expressions to find a `Call` to a candidate function. Returns the `func_id`
/// index if found.
///
/// Also looks through `Seq` whose last instruction is the value-producing one.
/// `LocalSet`-from-`Call` site-effects are *always* wrapped in a `Seq` in WIR —
/// e.g. `LocalSet(name, Seq([prefix..., Call(f)]))` — even when the unparser
/// flattens the prefix into separate statements. Without `Seq` unwrapping,
/// `find_nested_candidate_calls` would mis-classify every such site as a
/// "nested candidate call" and invalidate `f`, even though the pattern is the
/// idiomatic LocalSet-bound call we want to support.
fn unwrap_to_candidate_call(instr: &WirInstr, candidate_ids: &IndexSet<u32>) -> Option<u32> {
    match instr {
        WirInstr::Call { func_id, .. } if candidate_ids.contains(&func_id.index()) => {
            Some(func_id.index())
        }
        // Trivial block from inlining: the block's result value is either:
        // 1. The last instruction in body (implicit value)
        // 2. A Seq([..., value, Br]) pattern (break-with-value)
        WirInstr::Block { body, .. } => extract_block_result_call(body, candidate_ids),
        // Seq's value is the last instruction; preceding items are side
        // effects evaluated before the call.
        WirInstr::Seq(body) => body
            .last()
            .and_then(|last| unwrap_to_candidate_call(last, candidate_ids)),
        _ => None,
    }
}

/// Extract a candidate call from the result position of a block body.
/// Handles both implicit block results and explicit `Seq([value, Br])` patterns.
///
/// Returns `None` if the prefix instructions (everything before the result)
/// contain `Br` instructions that target the block itself.  Removing the block
/// wrapper in that case would corrupt those branch depths.
fn extract_block_result_call(body: &[WirInstr], candidate_ids: &IndexSet<u32>) -> Option<u32> {
    // Skip trailing Unreachable — translate_stmts_as_value appends Unreachable after
    // break-with-value statements so the Wasm validator sees no fallthrough value.
    // That trailing Unreachable is dead code and must not prevent SROA.
    let effective_body = if matches!(body.last(), Some(WirInstr::Unreachable)) {
        &body[..body.len() - 1]
    } else {
        body
    };
    let body = effective_body;
    let last = body.last()?;

    // Check prefix instructions for branches targeting this block.
    // Any `Br` in the prefix that targets this block (at relative depth 0
    // from the block scope, accounting for nested if/block/loop) would become
    // invalid once the block wrapper is removed.
    let prefix = &body[..body.len() - 1];
    if instrs_have_br_at_depth(prefix, 0) {
        return None;
    }

    match last {
        // Block ends with Seq([..., value, Br { depth }]) — break-with-value
        WirInstr::Seq(seq) => {
            if let Some((WirInstr::Br { .. }, rest)) = seq.split_last()
                && let Some((val, _)) = rest.split_last()
            {
                // Also check Seq items before the value for branches targeting the block.
                let seq_prefix = &rest[..rest.len() - 1];
                if instrs_have_br_at_depth(seq_prefix, 0) {
                    return None;
                }
                return unwrap_to_candidate_call(val, candidate_ids);
            }
            None
        }
        // Block ends with the value directly (no explicit br)
        other => unwrap_to_candidate_call(other, candidate_ids),
    }
}

/// Check if any instruction in the slice contains a `Br` that targets the block
/// at `target_depth` levels above the current nesting position.
///
/// `target_depth` is 0 when checking from directly inside the block.
/// Nested `if`/`block`/`loop` increase the depth by 1 for their bodies.
fn instrs_have_br_at_depth(instrs: &[WirInstr], target_depth: u32) -> bool {
    instrs
        .iter()
        .any(|instr| instr_has_br_at_depth(instr, target_depth))
}

fn instr_has_br_at_depth(instr: &WirInstr, target_depth: u32) -> bool {
    match instr {
        WirInstr::Br { depth } => *depth == target_depth,
        WirInstr::BrIf { depth, condition } => {
            *depth == target_depth || instr_has_br_at_depth(condition, target_depth)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets.contains(&target_depth)
                || *default == target_depth
                || instr_has_br_at_depth(index, target_depth)
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            instr_has_br_at_depth(condition, target_depth)
                || instrs_have_br_at_depth(then_body, target_depth + 1)
                || else_body
                    .as_ref()
                    .is_some_and(|eb| instrs_have_br_at_depth(eb, target_depth + 1))
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            instrs_have_br_at_depth(body, target_depth + 1)
        }
        WirInstr::Seq(items) => instrs_have_br_at_depth(items, target_depth),
        // Other instructions cannot introduce a label, but their operands can
        // still embed control flow (e.g. a `BranchHint`-wrapped condition or a
        // labeled block in value position), so recurse at the same depth — the
        // label-introducing arms above adjust it where needed.
        other => {
            let mut found = false;
            other.for_each_child(&mut |child| {
                found = found || instr_has_br_at_depth(child, target_depth);
            });
            found
        }
    }
}

/// Check if an instruction uses a candidate call result in an invalid way.
/// Invalid: Call to candidate as a nested expression (not direct child of
/// `LocalSet` or `Return` in a candidate caller).
fn check_invalid_call_uses(
    instr: &WirInstr,
    root_body: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
    caller_is_candidate: bool,
) {
    match instr {
        // LocalSet { value: <wrapper>(Call) } is valid — handled separately
        WirInstr::LocalSet { value, .. }
            if unwrap_to_candidate_call(value, candidate_ids).is_some() =>
        {
            // Check args of the underlying call for nested candidate calls
            if let Some(call) = unwrap_to_inner_call(value)
                && let WirInstr::Call { args, .. } = call
            {
                for arg in args {
                    find_nested_candidate_calls(arg, candidate_ids, invalid);
                }
            }
            // Also check prefix instructions in any block wrapper.
            // When the call is wrapped in Block { body: [prefix..., result_call] },
            // the prefix instructions may themselves contain SROA-compatible
            // `LocalSet(temp, Call(candidate))` shapes that should be accepted,
            // not invalidated. The prefix walk reuses the standard call-site
            // validator so those patterns are recognised.
            find_candidate_calls_in_block_prefix(
                value,
                root_body,
                candidate_ids,
                invalid,
                caller_is_candidate,
            );
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
            if caller_is_candidate && unwrap_to_candidate_call(v, candidate_ids).is_some() =>
        {
            if let Some(call) = unwrap_to_inner_call(v)
                && let WirInstr::Call { args, .. } = call
            {
                for arg in args {
                    find_nested_candidate_calls(arg, candidate_ids, invalid);
                }
            }
            find_candidate_calls_in_block_prefix(
                v,
                root_body,
                candidate_ids,
                invalid,
                caller_is_candidate,
            );
        }
        // Any other instruction that contains a Call to a candidate is invalid
        _ => {
            find_nested_candidate_calls(instr, candidate_ids, invalid);
        }
    }
}

/// Scan prefix instructions in `Block` / `Seq` wrappers for invalid candidate
/// call sites. When a `LocalSet { value: Block { body } }` or
/// `LocalSet { value: Seq(body) }` wraps a candidate call as its result, the
/// prefix instructions in the wrapper's body form their own statement-list
/// scope and may themselves contain SROA-compatible call sites.
///
/// Rather than unconditionally invalidating every candidate call in the
/// prefix, recurse into [`validate_call_sites_in_body`] so prefix items that
/// match `LocalSet(temp, Call(candidate))` with variant-access uses of `temp`
/// across `root_body` are accepted. The matching rewriter recurses through
/// `take_call_from_local_set` → `rewrite_call_sites` on the extracted prefix
/// so those accepted sites get rewritten.
fn find_candidate_calls_in_block_prefix(
    instr: &WirInstr,
    root_body: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
    caller_is_candidate: bool,
) {
    // The trailing-Unreachable trim is Block-specific:
    // `translate_stmts_as_value` appends `Unreachable` after a
    // break-with-value statement so the Wasm validator sees no
    // fallthrough value, and that `Unreachable` is dead code that must
    // not be treated as the result. `Seq` has no equivalent emitter
    // convention, so a trailing `Unreachable` inside `Seq([..., Call, Unreachable])`
    // is meant as "execute the Call then trap" — `Call` is genuinely
    // the prefix that needs scanning. Don't drop the Unreachable there.
    let (body, drop_trailing_unreachable) = match instr {
        WirInstr::Block { body, .. } => (body.as_slice(), true),
        WirInstr::Seq(body) => (body.as_slice(), false),
        _ => return,
    };
    let effective_body =
        if drop_trailing_unreachable && matches!(body.last(), Some(WirInstr::Unreachable)) {
            &body[..body.len() - 1]
        } else {
            body
        };
    if let Some((_, prefix)) = effective_body.split_last() {
        validate_call_sites_in_body(
            prefix,
            root_body,
            candidate_ids,
            invalid,
            caller_is_candidate,
        );
    }
}

/// Unwrap through `Block` or `Seq` to find the inner `Call` instruction (for
/// arg checking).
fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match instr {
        WirInstr::Call { .. } => Some(instr),
        WirInstr::Seq(body) => body.last().and_then(unwrap_to_inner_call),
        WirInstr::Block { body, .. } => {
            let last = body.last()?;
            match last {
                WirInstr::Seq(seq) => {
                    if let Some((WirInstr::Br { .. }, rest)) = seq.split_last()
                        && let Some((val, _)) = rest.split_last()
                    {
                        return unwrap_to_inner_call(val);
                    }
                    None
                }
                other => unwrap_to_inner_call(other),
            }
        }
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

/// Check that every reference to `local_name` in the instruction list is a
/// `StructGet { expr: LocalGet(local_name) }` — i.e., the local is never used
/// directly, only for field extraction.
fn apply_sroa(module: &mut WirPackage, confirmed: &[(u32, SroaCandidate)]) {
    // Build a lookup from func_id_index → candidate info
    let candidate_map: crate::hashmap::IndexMap<u32, &SroaCandidate> =
        confirmed.iter().map(|(id, c)| (*id, c)).collect();

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
            rewrite_call_sites(body, &candidate_map, &module.types);
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
            // For all other instructions (LocalSet, ValueCopy, etc.),
            // recurse into any nested children that might contain Return.
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
            if body.iter().any(WirInstr::always_diverges) {
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
                WirInstr::Block { body, .. } => {
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

/// Produce a default (zero) value for a given WIR type.
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
        WirType::Ref { .. } | WirType::AbstractRef { .. } => WirInstr::RefNull {
            heap_type: crate::wir::WirAbstractHeapType::None,
        },
        _ => WirInstr::I32Const(0), // fallback
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
                if is_candidate_call_set(&instrs[i + 1], dn, candidate_map) {
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
            extract_candidate_call_info(&instrs[set_idx], candidate_map)
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

        let vi = &candidate.variant_info;
        {
            // Variant candidate: build VariantReplacement
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
                                field_to_local.insert(
                                    (*case_type_idx, field.name.clone()),
                                    disc_local.clone(),
                                );
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
                                    field_to_local.insert(
                                        (*case_type_idx, field.name.clone()),
                                        sroa_local.clone(),
                                    );
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
            if let Some(WirTypeDef::Variant(wv)) = types.get(candidate.struct_type_idx as usize) {
                for (disc_val_2, case_type_opt_2) in vi.case_type_indices.iter().enumerate() {
                    if case_type_opt_2.is_none() {
                        continue;
                    }
                    // Locate the corresponding variant case by discriminant value.
                    let Some(wir_case) = wv.cases.iter().find(|c| c.index as usize == disc_val_2)
                    else {
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

            variant_replacements.insert(
                temp_name,
                VariantReplacement {
                    disc_local,
                    case_disc_values,
                    field_to_local,
                    ref_locals,
                },
            );
        }

        // Extract the Call instruction (and any prefix statements from block wrappers)
        let (mut prefix_instrs, call_instr) = take_call_from_local_set(&mut instrs[set_idx]);
        // Recursively rewrite the prefix so any nested
        // `LocalSet(temp, Call(candidate))` shapes it contains are also
        // turned into `MultiValueLocalBind` (and their variant-access
        // sites replaced). The validator's
        // `find_candidate_calls_in_block_prefix` accepts these patterns,
        // so the rewriter must follow through — otherwise the inner
        // candidate's signature would be multi-value while its call site
        // stays single-value, causing a Wasm arity mismatch.
        rewrite_call_sites(&mut prefix_instrs, candidate_map, types);
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
            recurse_rewrite_call_sites(instr, candidate_map, types);
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
        recurse_rewrite_call_sites(instr, candidate_map, types);
    }
}

/// Recurse into nested instruction bodies for call site rewriting.
fn recurse_rewrite_call_sites(
    instr: &mut WirInstr,
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
    types: &[WirTypeDef],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_call_sites(body, candidate_map, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            recurse_rewrite_call_sites(condition, candidate_map, types);
            rewrite_call_sites(then_body, candidate_map, types);
            if let Some(eb) = else_body {
                rewrite_call_sites(eb, candidate_map, types);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_call_sites(body, candidate_map, types);
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                recurse_rewrite_call_sites(child, candidate_map, types);
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
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
) -> bool {
    let WirInstr::LocalSet { name, value } = instr else {
        return false;
    };
    if name != expected_name {
        return false;
    }
    let candidate_ids: IndexSet<u32> = candidate_map.keys().copied().collect();
    unwrap_to_candidate_call(value, &candidate_ids).is_some()
}

/// Extract (`func_id_index`, `temp_name`) from a candidate call `LocalSet`.
/// Handles calls wrapped in `ValueCopy` or trivial inlined `Block`.
fn extract_candidate_call_info(
    instr: &WirInstr,
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
) -> Option<(u32, String)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    let candidate_ids: IndexSet<u32> = candidate_map.keys().copied().collect();
    unwrap_to_candidate_call(value, &candidate_ids).map(|idx| (idx, name.clone()))
}

/// Take the Call instruction out of a `LocalSet`, unwrapping through
/// `ValueCopy` and trivial `Block` wrappers. Replaces the instruction with Nop.
/// Returns `(prefix_instrs, call_instr)` where prefix instructions are statements
/// from inside Block wrappers that must be emitted before the call (e.g. initialization
/// of locals used as call arguments).
fn take_call_from_local_set(instr: &mut WirInstr) -> (Vec<WirInstr>, Box<WirInstr>) {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let mut prefix = Vec::new();
    let call = unwrap_and_take_call(*value, &mut prefix);
    (prefix, Box::new(call))
}

/// Recursively unwrap `Block` / `Seq` wrappers to extract the `Call`
/// instruction. Collects any non-result instructions from the wrappers into
/// `prefix` so they can be emitted before the call. Mirrors the
/// `unwrap_to_candidate_call` recognition path used during validation.
fn unwrap_and_take_call(instr: WirInstr, prefix: &mut Vec<WirInstr>) -> WirInstr {
    let mut current = instr;
    loop {
        match current {
            WirInstr::Call { .. } => return current,
            WirInstr::Block { ref mut body, .. } => {
                // Extract the call from the block's result position,
                // and collect all preceding statements as prefix.
                if let Some(call) = take_block_result_call(body, prefix) {
                    current = *call;
                } else {
                    unreachable!("expected call in SROA block unwrap");
                }
            }
            WirInstr::Seq(mut body) => {
                if body.is_empty() {
                    unreachable!("expected non-empty Seq in SROA call unwrap");
                }
                let last_idx = body.len() - 1;
                for item in &mut body[..last_idx] {
                    let taken = std::mem::replace(item, WirInstr::Nop);
                    if !matches!(taken, WirInstr::Nop) {
                        prefix.push(taken);
                    }
                }
                let last = std::mem::replace(&mut body[last_idx], WirInstr::Nop);
                current = last;
            }
            _ => unreachable!("unexpected instruction in SROA call unwrap"),
        }
    }
}

/// Take the call instruction from the result position of a block body.
/// Preceding statements in the block are moved into `prefix`.
fn take_block_result_call(
    body: &mut [WirInstr],
    prefix: &mut Vec<WirInstr>,
) -> Option<Box<WirInstr>> {
    if body.is_empty() {
        return None;
    }

    // Skip trailing Unreachable — translate_stmts_as_value may append one after a
    // break-with-value; it is dead code and must not be treated as the result value.
    let effective_len = if matches!(body.last(), Some(WirInstr::Unreachable)) {
        body.len() - 1
    } else {
        body.len()
    };
    if effective_len == 0 {
        return None;
    }
    let last_idx = effective_len - 1;

    // Move all statements before the last (result-producing) instruction to prefix
    for item in &mut body[..last_idx] {
        let taken = std::mem::replace(item, WirInstr::Nop);
        if !matches!(taken, WirInstr::Nop) {
            prefix.push(taken);
        }
    }

    let last = &mut body[last_idx];
    match last {
        // Seq([..., value, Br]) — take the value before Br, move others to prefix
        WirInstr::Seq(seq) => {
            if seq.len() >= 2 && matches!(seq.last(), Some(WirInstr::Br { .. })) {
                let val_idx = seq.len() - 2;
                // Move any statements before the value expression to prefix
                for item in &mut seq[..val_idx] {
                    let taken = std::mem::replace(item, WirInstr::Nop);
                    if !matches!(taken, WirInstr::Nop) {
                        prefix.push(taken);
                    }
                }
                let taken = std::mem::replace(&mut seq[val_idx], WirInstr::Nop);
                Some(Box::new(taken))
            } else {
                None
            }
        }
        // Last instruction is the value directly
        other => {
            let taken = std::mem::replace(other, WirInstr::Nop);
            Some(Box::new(taken))
        }
    }
}
