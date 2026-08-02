//! Phase 0: normalise the return-only temps NIR's `field_scalarize` leaves, so
//! the return-shape analysis sees the `Return(StructNew)` it understands.
//!
//! Two rewrites over the same pair shape: [`elide_return_only_temps`] relocates
//! the whole value to the `Return`, and [`scalarize_return_only_temps`] pins the
//! operands in temps and moves only the allocation.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirFunction, WirInstr, WirPackage, WirType, WirTypeDef};

use super::super::util::is_root_observable;
use super::layout::compute_variant_layout;

/// Phase 0b: pin a return-only temp's `StructNew` operands in per-field temps
/// and move the bare construction to the `Return`, restoring the
/// `Return(StructNew)` shape [`find_sroa_candidates`] accepts.
///
/// ```text
/// __hfs_call_N = struct.new Err { discriminant: 1, payload_0: eof(..) };
/// self.pos = __hfs_pos_X;
/// return __hfs_call_N;
///
/// // becomes
/// __scv_… = 1; __scv_… = eof(..);
/// self.pos = __hfs_pos_X;
/// return struct.new Err { discriminant: __scv_…, payload_0: __scv_… };
/// ```
///
/// Soundness: unlike [`elide_return_only_temps`], nothing observable moves.
/// Operands stay at their original program point, preserving evaluation order
/// and trap position; only the `struct.new` — allocation-only, state-free and
/// trap-free — is deferred. The intervening statements must still not reference
/// the temp, which [`find_paired_return`] checks.
pub(super) fn scalarize_return_only_temps(module: &mut WirPackage, pinned: &IndexSet<u32>) {
    // Planned up front: `scalarize_case_types` borrows the module immutably.
    let case_types: Vec<Option<IndexSet<u32>>> = module
        .functions
        .iter()
        .enumerate()
        .map(|(i, func)| {
            let func_id_index = module.defined_func_base + u32::try_from(i).unwrap();
            scalarize_case_types(module, func, func_id_index, pinned)
        })
        .collect();

    for (i, cases) in case_types.into_iter().enumerate() {
        let Some(cases) = cases else {
            continue;
        };
        let Some(mut body) = module.functions[i].body.take() else {
            continue;
        };
        scalarize_body(&mut body, &cases, &module.types);
        module.functions[i].body = Some(body);
    }
}

/// The variant case-struct type indices `func` may return, or `None` when the
/// function can't benefit (pinned, bodyless, or not returning a laid-out
/// variant). Restricting to these keeps Phase 0b from minting temps for a
/// `StructNew` no later phase would decompose.
fn scalarize_case_types(
    module: &WirPackage,
    func: &WirFunction,
    func_id_index: u32,
    pinned: &IndexSet<u32>,
) -> Option<IndexSet<u32>> {
    if pinned.contains(&func_id_index) || func.body.is_none() {
        return None;
    }
    let WirTypeDef::Func(func_type) = module.types.get(func.type_id.index() as usize)? else {
        return None;
    };
    if func_type.results.len() != 1 {
        return None;
    }
    let WirType::Ref { type_id, .. } = &func_type.results[0] else {
        return None;
    };
    let variant_type_idx = type_id.index();
    let WirTypeDef::Variant(variant_type) = module.types.get(variant_type_idx as usize)? else {
        return None;
    };
    let layout = compute_variant_layout(module, variant_type_idx, variant_type)?;
    Some(layout.valid_case_type_indices)
}

fn scalarize_body(body: &mut Vec<WirInstr>, cases: &IndexSet<u32>, types: &[WirTypeDef]) {
    let mut stats: IndexMap<String, ReturnTempStats> = IndexMap::default();
    scan_return_temp_stats(body, PairMode::Scalarize, &mut stats);

    let valid: IndexSet<String> = stats
        .iter()
        .filter(|(_, s)| s.is_return_only())
        .map(|(name, _)| name.clone())
        .collect();
    if valid.is_empty() {
        return;
    }
    scalarize_pairs(body, &valid, cases, types);
}

/// The `(name, type)` list a case struct's `StructNew` operands bind to, or
/// `None` when `type_idx` isn't one of `cases` or its arity disagrees.
fn case_field_bindings(
    type_idx: u32,
    operand_count: usize,
    cases: &IndexSet<u32>,
    types: &[WirTypeDef],
) -> Option<Vec<(String, WirType)>> {
    if !cases.contains(&type_idx) {
        return None;
    }
    let WirTypeDef::Struct(case_ty) = types.get(type_idx as usize)? else {
        return None;
    };
    if case_ty.fields.len() != operand_count {
        return None;
    }
    Some(
        case_ty
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect(),
    )
}

/// Plan the `LocalSet(temp, X) ; [intervening] ; Return(LocalGet(temp))` pairs of
/// one statement list as `set index → return index`. Read-only, so neither
/// rewrite re-derives its own analysis while mutating, and both agree on what a
/// pair is. `accept_value` adds the mode's demand on `X`.
fn plan_return_temp_pairs(
    instrs: &[WirInstr],
    valid: &IndexSet<String>,
    mode: PairMode,
    accept_value: impl Fn(&WirInstr) -> bool,
) -> IndexMap<usize, usize> {
    let mut pairs: IndexMap<usize, usize> = IndexMap::default();
    let mut i = 0;
    while i < instrs.len() {
        if let WirInstr::LocalSet { name, value } = &instrs[i]
            && valid.contains(name.as_str())
            && accept_value(value)
            && let Some(j) = find_paired_return(instrs, i, name, value, mode)
        {
            pairs.insert(i, j);
            i = j + 1;
            continue;
        }
        i += 1;
    }
    pairs
}

fn scalarize_pairs(
    instrs: &mut Vec<WirInstr>,
    valid: &IndexSet<String>,
    cases: &IndexSet<u32>,
    types: &[WirTypeDef],
) {
    // Planned against the original indices, which the rebuild below shifts.
    let pairs = plan_return_temp_pairs(instrs, valid, PairMode::Scalarize, |value| {
        matches!(value, WirInstr::StructNew { type_id, fields }
            if case_field_bindings(type_id.index(), fields.len(), cases, types).is_some())
    });
    if pairs.is_empty() {
        for instr in instrs.iter_mut() {
            scalarize_pairs_in_instr(instr, valid, cases, types);
        }
        return;
    }

    let mut result: Vec<WirInstr> = Vec::with_capacity(instrs.len() + pairs.len() * 2);
    let mut i = 0;
    while i < instrs.len() {
        let Some(&return_idx) = pairs.get(&i) else {
            let mut instr = std::mem::replace(&mut instrs[i], WirInstr::Nop);
            scalarize_pairs_in_instr(&mut instr, valid, cases, types);
            result.push(instr);
            i += 1;
            continue;
        };

        let WirInstr::LocalSet { name, value } = std::mem::replace(&mut instrs[i], WirInstr::Nop)
        else {
            unreachable!("planned scalarize pair is not a LocalSet")
        };
        let WirInstr::StructNew { type_id, fields } = *value else {
            unreachable!("planned scalarize pair value is not a StructNew")
        };
        let bindings = case_field_bindings(type_id.index(), fields.len(), cases, types)
            .unwrap_or_else(|| unreachable!("planned scalarize pair lost its case bindings"));

        // Every field contributes: a *later* operand's write clobbers an
        // earlier one's `local.get` just as an intervening statement does.
        let mut clobbered: IndexSet<String> = IndexSet::default();
        let mut reads: IndexSet<String> = IndexSet::default();
        for operand in &fields {
            collect_local_io(operand, &mut reads, &mut clobbered);
        }
        for k in (i + 1)..return_idx {
            collect_local_io(&instrs[k], &mut reads, &mut clobbered);
        }

        // Keyed by case type: `Ok` and `Err` share `payload_0` but not its type.
        let mut operands: Vec<WirInstr> = Vec::with_capacity(fields.len());
        for (operand, (field_name, field_ty)) in fields.into_iter().zip(bindings) {
            if !operand_needs_spill(&operand, &clobbered) {
                operands.push(operand);
                continue;
            }
            let temp = format!("__scv_{name}_{}_{field_name}", type_id.index());
            result.push(WirInstr::DeclareLocal {
                name: temp.clone(),
                ty: field_ty.clone(),
            });
            result.push(WirInstr::LocalSet {
                name: temp.clone(),
                value: Box::new(operand),
            });
            operands.push(WirInstr::LocalGet {
                name: temp,
                result_ty: field_ty,
            });
        }

        for k in (i + 1)..return_idx {
            let mut instr = std::mem::replace(&mut instrs[k], WirInstr::Nop);
            scalarize_pairs_in_instr(&mut instr, valid, cases, types);
            result.push(instr);
        }
        result.push(WirInstr::Return {
            value: Some(Box::new(WirInstr::StructNew {
                type_id,
                fields: operands,
            })),
        });
        i = return_idx + 1;
    }
    *instrs = result;
}

/// Whether an operand must be pinned in a temp rather than re-evaluated at the
/// `Return`. A literal is invariant, and a bare `local.get` is too unless
/// `clobbered` holds its local. Everything else — a call, a heap read, anything
/// that can trap, notably the `ref.as_non_null` guarding a non-null field — is
/// order-sensitive.
fn operand_needs_spill(operand: &WirInstr, clobbered: &IndexSet<String>) -> bool {
    match operand {
        WirInstr::I32Const(_)
        | WirInstr::I64Const(_)
        | WirInstr::F32Const(_)
        | WirInstr::F64Const(_)
        | WirInstr::RefNull { .. } => false,
        WirInstr::LocalGet { name, .. } => clobbered.contains(name),
        _ => true,
    }
}

fn scalarize_pairs_in_instr(
    instr: &mut WirInstr,
    valid: &IndexSet<String>,
    cases: &IndexSet<u32>,
    types: &[WirTypeDef],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            scalarize_pairs(body, valid, cases, types);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            scalarize_pairs(then_body, valid, cases, types);
            if let Some(eb) = else_body {
                scalarize_pairs(eb, valid, cases, types);
            }
        }
        _ => {}
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

impl ReturnTempStats {
    /// Whether the temp exists only to ferry a value into a `Return`: every
    /// write is paired with one and nothing else touches it.
    fn is_return_only(&self) -> bool {
        !self.has_other_use && self.paired_writes == self.total_writes && self.total_writes > 0
    }
}

/// A root-position read of non-local state: relocating it past an intervening
/// statement could observe a different heap, global, or memory. No value
/// containing one passes [`reads_only_local_state`].
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
        // `for_each_child` reaches the bound instruction but not the targets.
        WirInstr::MultiValueLocalBind { instr, locals } => {
            writes.extend(locals.iter().flatten().cloned());
            collect_local_io(instr, reads, writes);
        }
        _ => {
            expr.for_each_child(&mut |child| collect_local_io(child, reads, writes));
        }
    }
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
pub(super) fn elide_return_only_temps(module: &mut WirPackage, pinned: &IndexSet<u32>) {
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
    let mut stats: IndexMap<String, ReturnTempStats> = IndexMap::default();
    scan_return_temp_stats(body, PairMode::Relocate, &mut stats);

    let valid: IndexSet<String> = stats
        .iter()
        .filter(|(_, s)| s.is_return_only())
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
    mode: PairMode,
    stats: &mut IndexMap<String, ReturnTempStats>,
) {
    let mut i = 0;
    while i < instrs.len() {
        if let WirInstr::LocalSet { name, value } = &instrs[i]
            && pairable_value(value, mode)
            && let Some(return_idx) = find_paired_return(instrs, i, name, value, mode)
        {
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.paired_writes += 1;
            // The pair's `value` may still contain other locals' uses; the
            // LocalGet of `name` inside the Return is *consumed* by the pair
            // and not counted here. Any other use of `name` inside `value`
            // (unusual but possible — e.g. `__hfs_call = f(__hfs_call)`) is a
            // disqualifier.
            scan_return_temp_uses_in_expr(value, Some(name.as_str()), mode, stats);
            // The intervening stmts (i+1 .. return_idx) were already
            // disjointness-checked by `find_paired_return`. Still walk them
            // for the surrounding scan: a LocalGet of *another* temp in those
            // stmts must still count as an "other read" for that other temp.
            for j in (i + 1)..return_idx {
                scan_return_temp_uses_in_instr(&instrs[j], mode, stats);
            }
            i = return_idx + 1;
            continue;
        }
        scan_return_temp_uses_in_instr(&instrs[i], mode, stats);
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

/// Which Phase-0 rewrite a `LocalSet ... Return(LocalGet)` pair is scanned for.
/// Selects both the pairable-value shape ([`pairable_value`]) and what
/// [`find_paired_return`] demands of the intervening statements.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PairMode {
    /// [`elide_return_only_temps`]: the value itself moves to the `Return`, so
    /// it must survive being reordered past the intervening statements.
    Relocate,
    /// [`scalarize_return_only_temps`]: only the `StructNew` moves, so the
    /// reorder constraints do not apply.
    Scalarize,
}

/// Whether a `LocalSet` value can start a pair in `mode`.
fn pairable_value(value: &WirInstr, mode: PairMode) -> bool {
    match mode {
        PairMode::Relocate => reads_only_local_state(value),
        PairMode::Scalarize => matches!(value, WirInstr::StructNew { .. }),
    }
}

/// Return the index of a `Return(LocalGet(name))` reachable from
/// `start_idx + 1` through intervening stmts that don't reference `name`.
///
/// [`PairMode::Relocate`] additionally requires those stmts to
///   - not write any local that `value` reads, and
///   - not read any local that `value` writes,
///
/// and pairs a trap-capable `value` only with the *adjacent* `Return`
/// (`start_idx + 1`): relocating a trap past intervening statements would
/// reorder it across their effects — in particular past a conditional
/// `return` / `br` exit, on whose path the trap would then be lost
/// entirely (`t = a / b; if c { return OTHER; } return t;`).
/// [`PairMode::Scalarize`] moves no operand, so neither applies.
///
/// Returns `None` when no such return exists within
/// [`RETURN_TEMP_INTERVENING_BUDGET`] stmts.
fn find_paired_return(
    instrs: &[WirInstr],
    start_idx: usize,
    name: &str,
    value: &WirInstr,
    mode: PairMode,
) -> Option<usize> {
    let mut x_reads: IndexSet<String> = IndexSet::default();
    let mut x_writes: IndexSet<String> = IndexSet::default();
    if mode == PairMode::Relocate {
        collect_local_io(value, &mut x_reads, &mut x_writes);
    }
    let value_may_trap = mode == PairMode::Relocate && relocated_value_may_trap(value);

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
    mode: PairMode,
    stats: &mut IndexMap<String, ReturnTempStats>,
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            scan_return_temp_stats(body, mode, stats);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            scan_return_temp_uses_in_expr(condition, None, mode, stats);
            scan_return_temp_stats(then_body, mode, stats);
            if let Some(eb) = else_body {
                scan_return_temp_stats(eb, mode, stats);
            }
        }
        WirInstr::Seq(body) => {
            scan_return_temp_stats(body, mode, stats);
        }
        WirInstr::LocalSet { name, value } => {
            // Unpaired LocalSet (the pair branch would have handled it).
            // Count the write but disqualify the temp.
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.has_other_use = true;
            scan_return_temp_uses_in_expr(value, None, mode, stats);
        }
        WirInstr::LocalTee { name, value } => {
            // LocalTee both writes and leaves the value on the stack, so the
            // temp is observably read at the same time. Always disqualifies.
            let entry = stats.entry(name.clone()).or_default();
            entry.total_writes += 1;
            entry.has_other_use = true;
            scan_return_temp_uses_in_expr(value, None, mode, stats);
        }
        other => scan_return_temp_uses_in_expr(other, None, mode, stats),
    }
}

fn scan_return_temp_uses_in_expr(
    expr: &WirInstr,
    skip_name: Option<&str>,
    mode: PairMode,
    stats: &mut IndexMap<String, ReturnTempStats>,
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
        scan_return_temp_uses_in_instr(expr, mode, stats);
        return;
    }
    if let WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } = expr {
        // LocalSet / LocalTee appearing as a sub-expression (not a top-level
        // stmt) is always a disqualifier — it's not the paired shape.
        let entry = stats.entry(name.clone()).or_default();
        entry.total_writes += 1;
        entry.has_other_use = true;
        scan_return_temp_uses_in_expr(value, None, mode, stats);
        return;
    }
    if let WirInstr::Return { value: Some(rv) } = expr {
        // Bare `Return(LocalGet(name))` in expression position (shouldn't
        // arise structurally — Return is a statement — but defend anyway).
        if let WirInstr::LocalGet { name, .. } = rv.as_ref() {
            stats.entry(name.clone()).or_default().has_other_use = true;
            return;
        }
        scan_return_temp_uses_in_expr(rv, None, mode, stats);
        return;
    }
    expr.for_each_child(&mut |child| scan_return_temp_uses_in_expr(child, None, mode, stats));
}

/// Rewrite every paired `LocalSet(name, X); [intervening]; Return(LocalGet(name))`
/// where `name` is in `valid` into `Nop; [intervening]; Return(X)`. The
/// original `LocalSet`'s value moves into the `Return`; the `LocalSet`
/// slot becomes a `Nop` so downstream cleanup passes can drop it.
fn rewrite_return_temp_pairs(instrs: &mut [WirInstr], valid: &IndexSet<String>) {
    let pairs = plan_return_temp_pairs(instrs, valid, PairMode::Relocate, reads_only_local_state);
    let mut consumed: IndexSet<usize> = IndexSet::default();
    for (&set_idx, &return_idx) in &pairs {
        let WirInstr::LocalSet { value, .. } =
            std::mem::replace(&mut instrs[set_idx], WirInstr::Nop)
        else {
            unreachable!("planned relocate pair is not a LocalSet")
        };
        instrs[return_idx] = WirInstr::Return { value: Some(value) };
        consumed.insert(set_idx);
        consumed.insert(return_idx);
    }
    // The relocated value moved to the `Return`, so only the statements no pair
    // consumed still hold nested bodies to descend into.
    for (i, instr) in instrs.iter_mut().enumerate() {
        if !consumed.contains(&i) {
            rewrite_return_temp_pairs_in_instr(instr, valid);
        }
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
