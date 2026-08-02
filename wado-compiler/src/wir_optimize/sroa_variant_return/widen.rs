//! Widen a function whose sole result is a boxed variant into the flat result
//! vector [`compute_variant_layout`](super::layout::compute_variant_layout)
//! describes: candidate discovery, call-site validation, then the rewrite of
//! both the returns and every call site.
//!
//! Discovery propagates through tail calls, so a helper whose body ends in
//! `return another_parse(...)` qualifies alongside the candidate it forwards
//! to, instead of staying boxed for want of a `StructNew` at the leaf.

use crate::compiler_trace;
use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{
    WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId, WirVariantType,
};

use super::access::{
    LocalDefUse, VariantReplacement, all_uses_are_variant_access, build_variant_replacement,
    collect_refcast_aliases, replace_variant_accesses,
};
use super::layout::{VariantSroaInfo, compute_variant_layout, pad_variant_fields};
use super::wrapper::{
    ResultStep, any_branch_targets_enclosing, resolve_wrapped_result, take_call_from_local_set,
    unwrap_to_candidate_call, unwrap_to_inner_call,
};

/// Information about a variant-return SROA candidate function.
#[derive(Clone)]
pub(super) struct SroaCandidate {
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
    /// Field names for the multi-value results:
    /// `["discriminant", "payload_0", "payload_1", ...]`.
    field_names: Vec<String>,
    /// Variant-specific layout info.
    variant_info: VariantSroaInfo,
}

/// Per-function info computed up-front so the fix-point loop over candidate
/// discovery can re-check return shapes without redoing the layout analysis.
struct PotentialCandidate {
    func_id_index: u32,
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
pub(super) fn find_sroa_candidates(
    module: &WirPackage,
    pinned: &IndexSet<u32>,
) -> Vec<(u32, SroaCandidate)> {
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
            && let Some(candidate) = analyze_variant_layout(module, i, ret_type_idx, variant_type)
        {
            potentials.push(PotentialCandidate {
                func_id_index,
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
        let mut accepted_by_variant: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
        for p in &potentials {
            if accepted.contains(&p.func_id_index) {
                accepted_by_variant
                    .entry(p.candidate.struct_type_idx)
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
                .get(&p.candidate.struct_type_idx)
                .unwrap_or(&empty);
            if !all_returns_are_variant_struct_new(
                body,
                &p.candidate.valid_case_type_indices,
                tail_call_set,
            ) {
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
) -> Option<SroaCandidate> {
    let layout = compute_variant_layout(module, variant_type_idx, variant_type)?;
    Some(SroaCandidate {
        func_array_idx,
        struct_type_idx: variant_type_idx,
        valid_case_type_indices: layout.valid_case_type_indices,
        field_types: layout.field_types,
        field_names: layout.field_names,
        variant_info: layout.variant_info,
    })
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

/// Whether this value position is dead: an `Unreachable`, or a `Seq` with one
/// anywhere in it. A dead exit value needs no variant shape.
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
    case_types_by_candidate: &'a IndexMap<u32, IndexSet<u32>>,
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
pub(super) fn validate_call_sites(
    module: &WirPackage,
    candidates: &[(u32, SroaCandidate)],
) -> Vec<(u32, SroaCandidate)> {
    let mut candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();

    let case_types_by_candidate: IndexMap<u32, IndexSet<u32>> = candidates
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
            let mut effective_by_variant: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
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
pub(super) fn apply_sroa(module: &mut WirPackage, confirmed: &[(u32, SroaCandidate)]) {
    // Build a lookup from func_id_index → candidate info
    let candidate_map: IndexMap<u32, &SroaCandidate> =
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

/// Rewrite call sites of SROA'd functions.
///
/// For each `LocalSet { name: T, value: Call { func_id } }` where `func_id` is SROA'd:
/// 1. Replace the `LocalSet` with `MultiValueLocalBind` that binds results to fresh locals.
/// 2. For struct candidates: replace `StructGet { field, expr: LocalGet(T) }` with `LocalGet`.
/// 3. For variant candidates: replace `RefTest` with `I32Eq` and
///    `StructGet { RefCast { LocalGet(T) } }` with `LocalGet`.
fn rewrite_call_sites(
    instrs: &mut Vec<WirInstr>,
    candidate_map: &IndexMap<u32, &SroaCandidate>,
    candidate_ids: &IndexSet<u32>,
    types: &[WirTypeDef],
) {
    // Variant replacements: temp_name → VariantReplacement
    let mut variant_replacements: IndexMap<String, VariantReplacement> = IndexMap::default();

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
        let mut field_map: IndexMap<String, String> = IndexMap::default();
        let mut locals: Vec<Option<String>> = Vec::with_capacity(candidate.field_types.len());
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
        let mut refcast_aliases: IndexMap<String, (String, u32)> = IndexMap::default();
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
    candidate_map: &IndexMap<u32, &SroaCandidate>,
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
