//! Variant return SROA (Scalar Replacement of Aggregates) for WIR.
//!
//! Rewrites internal functions that return a variant lowered as
//! `(i32 disc, payload_0, payload_1, ...)` into Wasm multi-value returns,
//! eliminating GC struct allocation at function boundaries for the
//! variant-case ref.
//!
//! Tuple and user-struct return ABIs are classified at TIR
//! (`optimize::multi_value_return`); the variant case lives here because its
//! layout — shared versus per-case payload offsets — is WIR-specific.
//!
//! Two entry points over one layout engine ([`layout`]):
//!
//! - [`sroa_variant_returns`] widens a function whose sole result is a boxed
//!   `ref Variant`.
//! - [`flatten_variant_slots`] splits a `ref W` *result slot* whose `W` is
//!   itself a small variant — the `Ok(ref Option<T>)` a widening leaves boxed.

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

/// Widen every eligible variant-returning function, and every call site of one,
/// to the multi-value ABI.
pub(super) fn sroa_variant_returns(module: &mut WirPackage) {
    let pinned = collect_pinned_func_ids(module);
    elide_return_only_temps(module, &pinned);
    scalarize_return_only_temps(module, &pinned);

    let candidates = find_sroa_candidates(module, &pinned);
    compiler_trace!("sroa_variant_return", "candidates = {}", candidates.len());
    if candidates.is_empty() {
        return;
    }

    let confirmed = validate_call_sites(module, &candidates);
    compiler_trace!("sroa_variant_return", "confirmed = {}", confirmed.len());
    if confirmed.is_empty() {
        return;
    }

    apply_sroa(module, &confirmed);
}

/// Split every eligible nested `ref W` result slot, to a fix-point: a round
/// flattens at most one slot per function, exposing the next level for the round
/// after. It terminates on the arity cap — every flatten adds a field, and no
/// result vector exceeds `MAX_PER_CASE_RESULT_FIELDS`.
pub(super) fn flatten_variant_slots(module: &mut WirPackage) {
    let pinned = collect_pinned_func_ids(module);
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
