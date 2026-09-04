//! Nested variant-slot flattening. Single-level SROA leaves a `ref W` result
//! slot heap-boxed at every return when `W` is itself a small variant; this
//! phase splits it into `W`'s own `[inner_disc, payloads...]` layout via
//! [`compute_variant_layout`]. Bail-everywhere: expanded only when every return
//! decomposes and every slot local is consumed solely via variant access.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{
    WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId, WirVariantRepr,
};

use super::access::{
    LocalDefUse, VariantReplacement, all_uses_are_variant_access, build_variant_replacement,
    collect_refcast_aliases, replace_variant_accesses,
};
use super::layout::{
    MAX_PER_CASE_RESULT_FIELDS, VariantLayout, compute_variant_layout, default_value_for_type,
    pad_variant_fields,
};
use super::wrapper::unwrap_to_inner_call;

/// A confirmed slot-flatten target: result slot `slot` of function `func_idx`,
/// a `ref W`, expands into `W`'s multi-value layout.
pub(super) struct SlotFlattenCand {
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
/// return leaves the slot unused (a null ref, e.g. an `Err` return). Rather than
/// rely on those locals being dead, the discriminant is chosen to match *no*
/// payload-bearing case: the unit case's value if one exists, else the case
/// count. An admitted read then misses every case, as `ref.test` on null did.
fn default_variant_vector(cand: &SlotFlattenCand) -> Vec<WirInstr> {
    let vi = &cand.layout.variant_info;
    let disc = vi
        .case_type_indices
        .iter()
        .position(Option::is_none)
        .unwrap_or(vi.case_type_indices.len());
    let disc = i32::try_from(disc).unwrap_or(-1);
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
pub(super) fn slot_flatten_candidates(
    module: &WirPackage,
    pinned: &IndexSet<u32>,
) -> Vec<SlotFlattenCand> {
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
/// where `<slot extraction>` is `LocalGet(slot)` under `RefAsNonNull` wrappers.
/// `rewrite_unwrap_to_guard` discards the whole then-arm, so anything richer is
/// rejected, and `t` must have no reads outside the discarded pair.
fn then_is_pure_slot_copy(then_body: &[WirInstr], slot_local: &str, def_use: &LocalDefUse) -> bool {
    fn is_slot_extraction(e: &WirInstr, slot: &str) -> bool {
        match e {
            WirInstr::RefAsNonNull(inner) => is_slot_extraction(inner, slot),
            WirInstr::LocalGet { name, .. } => name == slot,
            _ => false,
        }
    }
    // The arm can arrive as one `Seq` node rather than as two statements of the
    // body slice — same shape, one level of nesting on.
    if let [WirInstr::Seq(inner)] = then_body {
        return then_is_pure_slot_copy(inner, slot_local, def_use);
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
/// rewritable. A *structural* check: every use must be a payload-case
/// `ref.test` / `ref.cast`, directly or through the `?`-unwrap alias. Dominance
/// is not required — an absent slot decodes via [`default_variant_vector`] to a
/// discriminant matching no payload case. `case_types` excludes unit cases.
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

/// True when `instr` (any position) contains a `Call` to a func id in `ids`.
fn body_calls_any(instr: &WirInstr, ids: &IndexSet<u32>) -> bool {
    if let WirInstr::Call { func_id, .. } = instr
        && ids.contains(&func_id.index())
    {
        return true;
    }
    let mut found = false;
    instr.for_each_child(&mut |c| found = found || body_calls_any(c, ids));
    found
}

/// Locals a call site's function may already hold before flattening stops
/// paying there.
///
/// Splicing the slot trades one heap object for `layout.len()` more values live
/// across the call. That is the trade SROA width already answers for elsewhere:
/// past the register file, values live across a call are spill slots reloaded at
/// every call boundary, and the allocation removed does not price them. Every
/// benchmark that gains here decodes through callers of at most 93 locals; the
/// one that loses, cbor-twitter, decodes `User` and `Status` at 307 and 186. The
/// cut sits between them — see the WEP for the measurements.
///
/// One caller over the cut declines the callee for *all* of them, not just that
/// site: the slot is part of the result signature, so it is flattened everywhere
/// or nowhere. What keeps that from being blunt is monomorphization —
/// `next_field<S>` is a distinct callee per struct, so a rule that has to answer
/// per callee still lands per decoded type.
const MAX_CALLER_LOCALS: usize = 128;

/// Phase 2: keep candidates whose every call site consumes the slot cleanly.
pub(super) fn validate_slot_sites(
    module: &WirPackage,
    cands: Vec<SlotFlattenCand>,
) -> Vec<SlotFlattenCand> {
    // `def_use` is only consulted for functions that hold a candidate call site,
    // so build it lazily for exactly those — the common leaf/stdlib function
    // that references no candidate skips both the map build and the scan below.
    let cand_ids: IndexSet<u32> = cands.iter().map(|c| c.func_id_index).collect();
    // `(def_use, declared locals)` for exactly those functions, both taken once
    // per function rather than once per candidate that lands in it. The local
    // count comes from `declared_locals`, the emitter's authority on what a
    // function holds, so the cut below is read in the unit the emitter allocates.
    let sites: Vec<Option<(LocalDefUse, usize)>> = module
        .functions
        .iter()
        .map(|func| {
            func.body
                .as_deref()
                .filter(|body| body.iter().any(|i| body_calls_any(i, &cand_ids)))
                .map(|body| {
                    (
                        LocalDefUse::of_body(body),
                        func.declared_locals().iter().count(),
                    )
                })
        })
        .collect();
    cands
        .into_iter()
        .filter(|cand| {
            let case_types = slot_case_types(cand);
            let mut saw_call = false;
            let mut all_ok = true;
            for (i, func) in module.functions.iter().enumerate() {
                let Some((def_use, locals)) = &sites[i] else {
                    continue;
                };
                let body = func.body.as_ref().unwrap();
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
                if mvbind_calls > 0 && *locals > MAX_CALLER_LOCALS {
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

/// Phase 3: rewrite signatures, returns, and call sites of confirmed slot
/// candidates.
pub(super) fn apply_slot_flatten(module: &mut WirPackage, confirmed: &[SlotFlattenCand]) {
    let by_func: IndexMap<u32, &SlotFlattenCand> =
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
    by_func: &IndexMap<u32, &SlotFlattenCand>,
    types: &[WirTypeDef],
) {
    // Plan: classify every candidate call site's slot consumer against the full
    // body. Keyed by slot local (single-level SROA gives each call site a unique
    // slot-local name).
    let mut plans: IndexMap<String, SlotConsumer> = IndexMap::default();
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
    let mut replacements: IndexMap<String, VariantReplacement> = IndexMap::default();
    expand_slot_binds(body, by_func, &plans, types, &mut replacements);
    // Replace variant accesses on the slot local / unwrap alias.
    let mut aliases: IndexMap<String, (String, u32)> = IndexMap::default();
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
    by_func: &IndexMap<u32, &SlotFlattenCand>,
    def_use: &LocalDefUse,
    plans: &mut IndexMap<String, SlotConsumer>,
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
    by_func: &IndexMap<u32, &SlotFlattenCand>,
    plans: &IndexMap<String, SlotConsumer>,
    types: &[WirTypeDef],
    replacements: &mut IndexMap<String, VariantReplacement>,
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
        let mut field_map: IndexMap<String, String> = IndexMap::default();
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

/// Drop the block result type along a diverging arm's tail chain.
///
/// The `?` desugar nests the error test as an `else if` carrying the binding's
/// type. Once the binding is gone the outer `if` is a statement, and an inner
/// arm still declaring a result leaves its value on the stack — "values
/// remaining on stack at end of block". Only a node that diverges is touched,
/// and a diverging one produces no value to begin with, so clearing the
/// declaration is what makes the type match what the arm actually does.
///
/// `Seq` is peeled for the same reason [`then_is_pure_slot_copy`] peels it: the
/// arm can arrive one level of nesting on, and `always_diverges` sees through
/// that, so stopping here would clear nothing and emit the invalid block.
fn drop_tail_result(body: &mut [WirInstr]) {
    let Some(last) = body.last_mut() else {
        return;
    };
    if !WirInstr::always_diverges(last) {
        return;
    }
    match last {
        WirInstr::If {
            result,
            then_body,
            else_body,
            ..
        } => {
            *result = None;
            drop_tail_result(then_body);
            if let Some(eb) = else_body {
                drop_tail_result(eb);
            }
        }
        WirInstr::Seq(inner) => drop_tail_result(inner),
        _ => {}
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
                let mut eb = else_body.take();
                if let Some(eb) = eb.as_mut() {
                    drop_tail_result(eb);
                }
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
