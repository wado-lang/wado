//! Nested result-slot flattening for variant returns. Widening one into
//! `[i32 disc, payload…]` happens at NIR; what stays here is the step needing
//! WIR shapes — splitting a `ref W` *result slot* whose `W` is itself a small
//! variant, the `Ok(ref Option<T>)` a widening leaves boxed. Its gates read
//! shapes NIR cannot see (WEP 2026-08-03).

use crate::compiler_trace;
use crate::wir::WirPackage;

use super::util::collect_pinned_func_ids;

mod access;
mod layout;
mod slot_flatten;
mod wrapper;

use slot_flatten::{apply_slot_flatten, slot_flatten_candidates, validate_slot_sites};

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
