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
use crate::wir::WirPackage;

use super::util::collect_pinned_func_ids;

mod access;
mod layout;
mod return_temp;
mod slot_flatten;
mod widen;
mod wrapper;

use return_temp::{elide_return_only_temps, scalarize_return_only_temps};
use slot_flatten::{apply_slot_flatten, slot_flatten_candidates, validate_slot_sites};
use widen::{apply_sroa, find_sroa_candidates, validate_call_sites};

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

    // Phase 0b: split what Phase 0a declined to relocate.
    scalarize_return_only_temps(module, &pinned);

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
