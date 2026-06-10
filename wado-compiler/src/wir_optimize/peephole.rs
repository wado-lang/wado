//! Peephole optimization pass for WIR.
//!
//! Per-function pass that applies local rewrites:
//! - Constant folding on integer comparisons
//! - Dead `If` elimination (constant condition)
//! - Redundant `ValueCopy` elision
//! - Copy-used-only-for-field-reads elision
//! - Sign-extension folding into loads / mask elision
//! - Redundant `ref.cast` / `ref.test` elimination
//! - `local.set` + first `local.get` fusion into `local.tee`
//!
//! These are Wasm-instruction-selection-level rewrites that have no NIR
//! analogue (`local.tee`, the signed-load variants, `ref.cast` redundancy
//! against the WIR static type), so they only make sense after lowering.

use crate::wir::{WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId};
use crate::wir_optimize::util::{is_side_effect_free, may_trap};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};
use indexmap::{IndexMap, IndexSet};

/// Per-local definition statistics gathered over a whole function body.
#[derive(Default, Clone)]
struct LocalDefInfo {
    /// Number of definitions: `LocalSet`, `LocalTee`, `MultiValueLocalBind` target.
    defs: u32,
    /// Number of `LocalTee` definitions (a tee is also a use, so tee'd locals
    /// are excluded from copy propagation).
    tees: u32,
}

/// Propagate trivial copies (`alias = source`) across a function: every use of
/// `alias` is replaced with `source`, then the now-dead copy is deleted.
///
/// A copy `LocalSet { alias, LocalGet { source } }` is propagated when:
/// - `alias` is single-def: this copy is its only `LocalSet`, it is never
///   `LocalTee`'d, and it is not a parameter. Every read of `alias` therefore
///   observes the value written by this copy.
/// - `source` has at most one definition and is never `LocalTee`'d, and that
///   definition (if any) structurally dominates the copy — or `source` is a
///   parameter. Together this means `source` is never rewritten between the
///   copy and the alias uses, so it is invariant over that span.
/// - the copy dominates every use of `alias` (checked structurally over the
///   WIR control flow), so no use can observe `alias`'s zero-initialized value
///   before the copy runs.
///
/// Under those conditions `alias == source` holds at every use of `alias`, so
/// the substitution is sound. Running before SROA also lets variant SROA see
/// direct RefTest/RefCast patterns on the original local.
pub(super) fn propagate_trivial_copies(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        let params: IndexSet<String> = func.param_names.iter().cloned().collect();
        propagate_copies_in_function(body, &params);
    }
}

fn propagate_copies_in_function(body: &mut [WirInstr], params: &IndexSet<String>) {
    // 1. Count definitions per local across the whole function.
    let mut counts: IndexMap<String, LocalDefInfo> = IndexMap::default();
    let mut counter = DefCounter {
        counts: &mut counts,
    };
    counter.visit_body(body);

    // 2. Collect candidate copies `alias -> source`.
    let mut candidates: IndexMap<String, String> = IndexMap::default();
    let mut collector = CopyCollector {
        counts: &counts,
        params,
        candidates: &mut candidates,
    };
    collector.visit_body(body);
    if candidates.is_empty() {
        return;
    }

    // 3. Drop candidates whose copy does not dominate every alias use, or
    //    whose source is not yet invariant at the copy.
    let mut disqualified: IndexSet<String> = IndexSet::default();
    let mut defined: IndexSet<String> = IndexSet::default();
    check_dominance_in_body(body, &candidates, params, &mut defined, &mut disqualified);
    candidates.retain(|alias, _| !disqualified.contains(alias));
    if candidates.is_empty() {
        return;
    }

    // 4. Resolve transitive chains (`a = b; b = c` makes both `a` and `b` map
    //    to `c`).
    let subst = resolve_chains(&candidates);

    // 5. Rewrite `LocalGet alias` to `LocalGet source` and delete the copies.
    apply_in_body(body, &subst);
}

/// Counts `LocalSet` / `LocalTee` / `MultiValueLocalBind` definitions per local.
struct DefCounter<'a> {
    counts: &'a mut IndexMap<String, LocalDefInfo>,
}

impl WirRefVisitor for DefCounter<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalSet { name, .. } => {
                self.counts.entry(name.clone()).or_default().defs += 1;
            }
            WirInstr::LocalTee { name, .. } => {
                let info = self.counts.entry(name.clone()).or_default();
                info.defs += 1;
                info.tees += 1;
            }
            WirInstr::MultiValueLocalBind { locals, .. } => {
                for local in locals.iter().flatten() {
                    self.counts.entry(local.clone()).or_default().defs += 1;
                }
            }
            _ => {}
        }
        self.walk_instr(instr);
    }
}

/// Collects `LocalSet { alias, LocalGet { source } }` copies eligible for
/// propagation, based on the whole-function definition counts.
struct CopyCollector<'a> {
    counts: &'a IndexMap<String, LocalDefInfo>,
    params: &'a IndexSet<String>,
    candidates: &'a mut IndexMap<String, String>,
}

impl WirRefVisitor for CopyCollector<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        if let WirInstr::LocalSet { name: alias, value } = instr
            && let WirInstr::LocalGet { name: source, .. } = value.as_ref()
            && alias != source
            && !self.params.contains(alias)
        {
            // `alias` must be single-def (this copy) and never tee'd, so every
            // read of it observes this copy.
            let alias_single_def = self
                .counts
                .get(alias)
                .is_some_and(|i| i.defs == 1 && i.tees == 0);
            // `source` must be single-assignment and never tee'd, so its value
            // is invariant across the function. A parameter with no definition
            // qualifies (`defs == 0`).
            let source_invariant = self
                .counts
                .get(source)
                .is_none_or(|i| i.defs <= 1 && i.tees == 0);
            if alias_single_def && source_invariant {
                self.candidates.insert(alias.clone(), source.clone());
            }
        }
        self.walk_instr(instr);
    }
}

/// Structural dominance check: walks the function in execution order tracking,
/// per scope, which locals have had a definition executed. It enforces two
/// conditions per candidate copy `alias = source`:
///
/// - the copy dominates every use of `alias` — a `LocalGet alias` reached
///   before the copy disqualifies `alias`, so no use observes the alias's
///   zero-initialized value;
/// - `source` is invariant from the copy onward — its definition must already
///   dominate the copy (or `source` is a parameter / never defined). Combined
///   with `source` having at most one definition (checked by `CopyCollector`),
///   this guarantees `source` is not rewritten between the copy and the alias
///   uses, so `alias == source` holds at every use.
///
/// `defined` is threaded by value into conditional and looping scopes
/// (`Block` / `Loop` / `If` branches) so a definition inside one of them does
/// not count as executed on paths that skip it. `Seq` is unconditional
/// straight-line code, so definitions inside it leak to the enclosing scope.
fn check_dominance_in_body(
    body: &[WirInstr],
    candidates: &IndexMap<String, String>,
    params: &IndexSet<String>,
    defined: &mut IndexSet<String>,
    disqualified: &mut IndexSet<String>,
) {
    for instr in body {
        check_dominance_in_instr(instr, candidates, params, defined, disqualified);
    }
}

fn check_dominance_in_instr(
    instr: &WirInstr,
    candidates: &IndexMap<String, String>,
    params: &IndexSet<String>,
    defined: &mut IndexSet<String>,
    disqualified: &mut IndexSet<String>,
) {
    match instr {
        WirInstr::LocalGet { name, .. } => {
            if candidates.contains_key(name) && !defined.contains(name) {
                disqualified.insert(name.clone());
            }
        }
        WirInstr::LocalSet { name, value } => {
            check_dominance_in_instr(value, candidates, params, defined, disqualified);
            // For a candidate copy, `source` must already be invariant here:
            // its definition must dominate the copy (or it is a parameter).
            // Otherwise the copy could capture an earlier value while `source`
            // is still rewritten before the alias is read.
            if let Some(source) = candidates.get(name)
                && !params.contains(source)
                && !defined.contains(source)
            {
                disqualified.insert(name.clone());
            }
            defined.insert(name.clone());
        }
        WirInstr::LocalTee { name, value } => {
            check_dominance_in_instr(value, candidates, params, defined, disqualified);
            defined.insert(name.clone());
        }
        WirInstr::MultiValueLocalBind { instr, locals } => {
            check_dominance_in_instr(instr, candidates, params, defined, disqualified);
            for local in locals.iter().flatten() {
                defined.insert(local.clone());
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            let mut nested = defined.clone();
            check_dominance_in_body(body, candidates, params, &mut nested, disqualified);
        }
        WirInstr::Seq(body) => {
            check_dominance_in_body(body, candidates, params, defined, disqualified);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            check_dominance_in_instr(condition, candidates, params, defined, disqualified);
            let mut then_defined = defined.clone();
            check_dominance_in_body(
                then_body,
                candidates,
                params,
                &mut then_defined,
                disqualified,
            );
            if let Some(eb) = else_body {
                let mut else_defined = defined.clone();
                check_dominance_in_body(eb, candidates, params, &mut else_defined, disqualified);
            }
        }
        other => {
            other.for_each_child(&mut |child| {
                check_dominance_in_instr(child, candidates, params, defined, disqualified);
            });
        }
    }
}

/// Collapse `a -> b -> c` chains so every alias maps to its ultimate source.
fn resolve_chains(candidates: &IndexMap<String, String>) -> IndexMap<String, String> {
    let mut subst: IndexMap<String, String> = IndexMap::default();
    for alias in candidates.keys() {
        let mut seen: IndexSet<String> = IndexSet::default();
        seen.insert(alias.clone());
        let mut current = candidates[alias].clone();
        while !seen.contains(&current) {
            seen.insert(current.clone());
            match candidates.get(&current) {
                Some(next) => current.clone_from(next),
                None => break,
            }
        }
        subst.insert(alias.clone(), current);
    }
    subst
}

fn apply_in_body(body: &mut [WirInstr], subst: &IndexMap<String, String>) {
    for instr in body.iter_mut() {
        apply_in_instr(instr, subst);
    }
}

fn apply_in_instr(instr: &mut WirInstr, subst: &IndexMap<String, String>) {
    match instr {
        // The single, trivial copy that defined this alias — now dead.
        WirInstr::LocalSet { name, .. } if subst.contains_key(name) => {
            *instr = WirInstr::Nop;
        }
        WirInstr::LocalGet { name, .. } => {
            if let Some(source) = subst.get(name) {
                *name = source.clone();
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            apply_in_body(body, subst);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            apply_in_instr(condition, subst);
            apply_in_body(then_body, subst);
            if let Some(eb) = else_body {
                apply_in_body(eb, subst);
            }
        }
        other => {
            other.for_each_boxed_child_mut(&mut |child| apply_in_instr(child, subst));
        }
    }
}

/// Recursively optimize a list of instructions.
///
/// First descends into nested instruction bodies (Block, Loop, If, Seq),
/// then applies flat-level optimizations on the current list.
///
/// Value-copy elision is a single whole-function pass (see
/// `elide_value_copies_whole_function`) rather than per-scope, because safe
/// elision requires knowing every trailing instruction reachable after the
/// copy — including those in enclosing scopes — and recursive per-scope calls
/// cannot observe them.
pub(super) fn run_peephole(instrs: &mut [WirInstr], types: &[WirTypeDef]) {
    for instr in instrs.iter_mut() {
        optimize_nested(instr, types);
    }
    fold_constant_comparisons(instrs);
    // Re-run dead branch elimination after constant folding may have turned
    // If conditions into I32Const(0)/I32Const(1).
    for instr in instrs.iter_mut() {
        eliminate_const_if(instr);
    }
    fold_eqz_patterns(instrs);
    fold_branchless_increment(instrs);
    simplify_redundant_byte_masks(instrs);
    fold_sign_extensions(instrs);
    simplify_redundant_ref_ops(instrs);
    relax_gc_operand_nullability(instrs);
    fuse_local_tee(instrs);
}

/// Recurse into nested instruction bodies and eliminate dead branches.
fn optimize_nested(instr: &mut WirInstr, types: &[WirTypeDef]) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            run_peephole(body, types);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            run_peephole(then_body, types);
            if let Some(eb) = else_body {
                run_peephole(eb, types);
            }
            eliminate_const_if(instr);
        }
        WirInstr::Seq(body) => {
            run_peephole(body, types);
        }
        WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. } => {
            optimize_nested(value, types);
        }
        WirInstr::Call { args, .. } => {
            for arg in args {
                optimize_nested(arg, types);
            }
        }
        WirInstr::CallIndirect { index, args, .. } => {
            optimize_nested(index, types);
            for arg in args {
                optimize_nested(arg, types);
            }
        }
        WirInstr::StructNew { fields, .. } => {
            for field in fields {
                optimize_nested(field, types);
            }
        }
        WirInstr::Drop(inner)
        | WirInstr::RefAsNonNull(inner)
        | WirInstr::RefCast { expr: inner, .. }
        | WirInstr::RefTest { expr: inner, .. }
        | WirInstr::StructGet { expr: inner, .. } => {
            optimize_nested(inner, types);
        }
        WirInstr::StructSet { expr, value, .. } => {
            optimize_nested(expr, types);
            optimize_nested(value, types);
        }
        WirInstr::Return { value: Some(v) } => {
            optimize_nested(v, types);
        }
        _ => {}
    }
}

/// Try to evaluate a WIR condition to a boolean constant. Looks through a
/// `BranchHint` wrapper — a hinted branch whose condition became constant
/// must still fold (the hint disappears with the branch).
fn try_fold_wir_to_bool(instr: &WirInstr) -> Option<bool> {
    match instr.peel_hint() {
        WirInstr::I32Const(v) => Some(*v != 0),
        _ => None,
    }
}

/// Recursively replace `If` with a constant condition by the surviving branch.
fn eliminate_const_if(instr: &mut WirInstr) {
    struct ElimConstIf;
    impl WirMutVisitor for ElimConstIf {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            if let WirInstr::If {
                condition,
                then_body,
                else_body,
                result,
            } = instr
                && let Some(const_val) = try_fold_wir_to_bool(condition)
            {
                if const_val {
                    let then_instrs = std::mem::take(then_body);
                    *instr = WirInstr::Block {
                        label: None,
                        result: result.clone(),
                        body: then_instrs,
                    };
                } else if let Some(eb) = else_body {
                    let else_instrs = std::mem::take(eb);
                    *instr = WirInstr::Block {
                        label: None,
                        result: result.clone(),
                        body: else_instrs,
                    };
                } else {
                    *instr = WirInstr::Block {
                        label: None,
                        result: None,
                        body: vec![WirInstr::Nop],
                    };
                }
            }
        }
    }
    ElimConstIf.visit_instr(instr);
}

/// Recursively fold constant integer comparisons to `I32Const`.
fn fold_constant_comparisons(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        fold_constant_comparisons_in_instr(instr);
    }
}

fn fold_constant_comparisons_in_instr(instr: &mut WirInstr) {
    struct FoldConstComparisons;
    impl WirMutVisitor for FoldConstComparisons {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            try_fold_comparison(instr);
        }
    }
    FoldConstComparisons.visit_instr(instr);
}

fn try_fold_comparison(instr: &mut WirInstr) {
    // Try to fold this instruction
    let result = match instr {
        WirInstr::I32GeS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv >= *rv)),
            _ => None,
        },
        WirInstr::I32GeU(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => {
                Some(i32::from(lv.cast_unsigned() >= rv.cast_unsigned()))
            }
            _ => None,
        },
        WirInstr::I32LtS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv < *rv)),
            _ => None,
        },
        WirInstr::I32GtS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv > *rv)),
            _ => None,
        },
        WirInstr::I32Eq(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv == *rv)),
            _ => None,
        },
        WirInstr::I32Ne(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv != *rv)),
            _ => None,
        },
        WirInstr::I32LeS(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => Some(i32::from(*lv <= *rv)),
            _ => None,
        },
        WirInstr::I32LtU(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => {
                Some(i32::from(lv.cast_unsigned() < rv.cast_unsigned()))
            }
            _ => None,
        },
        WirInstr::I32GtU(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => {
                Some(i32::from(lv.cast_unsigned() > rv.cast_unsigned()))
            }
            _ => None,
        },
        WirInstr::I32LeU(l, r) => match (l.as_ref(), r.as_ref()) {
            (WirInstr::I32Const(lv), WirInstr::I32Const(rv)) => {
                Some(i32::from(lv.cast_unsigned() <= rv.cast_unsigned()))
            }
            _ => None,
        },
        _ => None,
    };

    if let Some(val) = result {
        *instr = WirInstr::I32Const(val);
    }
}

/// Replace `I32Eq(expr, I32Const(0))` with `I32Eqz(expr)` (and i64 variant).
/// Also replace `I32Ne(expr, I32Const(0))` with `I32Eqz(I32Eqz(expr))` — this
/// is not done because it's not a win. Instead, only `== 0` patterns are folded.
fn fold_eqz_patterns(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        fold_eqz_in_instr(instr);
    }
}

fn fold_eqz_in_instr(instr: &mut WirInstr) {
    struct FoldEqz;
    impl WirMutVisitor for FoldEqz {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            try_fold_eqz(instr);
            try_negate_eqz_comparison(instr);
        }
    }
    FoldEqz.visit_instr(instr);
}

fn try_fold_eqz(instr: &mut WirInstr) {
    // i32.eq(expr, 0) → i32.eqz(expr)
    // i32.eq(0, expr) → i32.eqz(expr)
    match instr {
        WirInstr::I32Eq(l, r) => {
            if matches!(r.as_ref(), WirInstr::I32Const(0)) {
                let operand = std::mem::replace(l, Box::new(WirInstr::Nop));
                *instr = WirInstr::I32Eqz(operand);
            } else if matches!(l.as_ref(), WirInstr::I32Const(0)) {
                let operand = std::mem::replace(r, Box::new(WirInstr::Nop));
                *instr = WirInstr::I32Eqz(operand);
            }
        }
        WirInstr::I64Eq(l, r) => {
            if matches!(r.as_ref(), WirInstr::I64Const(0)) {
                let operand = std::mem::replace(l, Box::new(WirInstr::Nop));
                *instr = WirInstr::I64Eqz(operand);
            } else if matches!(l.as_ref(), WirInstr::I64Const(0)) {
                let operand = std::mem::replace(r, Box::new(WirInstr::Nop));
                *instr = WirInstr::I64Eqz(operand);
            }
        }
        _ => {}
    }
}

/// Fold `I32Eqz(comparison)` into the negated comparison instruction.
///
/// e.g., `i32.eqz(i32.le_s(a, b))` → `i32.gt_s(a, b)`
///
/// This saves one Wasm instruction per negated comparison, which is significant
/// in tight loops (e.g., `while x <= limit` lowers to `if !(x <= limit) break`).
/// Only applies to integer comparisons — float comparisons are excluded due to NaN.
fn try_negate_eqz_comparison(instr: &mut WirInstr) {
    let WirInstr::I32Eqz(inner) = instr else {
        return;
    };
    // Determine which negated constructor to use based on the inner comparison.
    // Take a reference first to decide, then destructure to move operands.
    let ctor: fn(Box<WirInstr>, Box<WirInstr>) -> WirInstr = match inner.as_ref() {
        // i32 signed
        WirInstr::I32LeS(..) => WirInstr::I32GtS,
        WirInstr::I32LtS(..) => WirInstr::I32GeS,
        WirInstr::I32GeS(..) => WirInstr::I32LtS,
        WirInstr::I32GtS(..) => WirInstr::I32LeS,
        // i32 unsigned
        WirInstr::I32LeU(..) => WirInstr::I32GtU,
        WirInstr::I32LtU(..) => WirInstr::I32GeU,
        WirInstr::I32GeU(..) => WirInstr::I32LtU,
        WirInstr::I32GtU(..) => WirInstr::I32LeU,
        // i32 eq/ne
        WirInstr::I32Eq(..) => WirInstr::I32Ne,
        WirInstr::I32Ne(..) => WirInstr::I32Eq,
        // i64 signed
        WirInstr::I64LeS(..) => WirInstr::I64GtS,
        WirInstr::I64LtS(..) => WirInstr::I64GeS,
        WirInstr::I64GeS(..) => WirInstr::I64LtS,
        WirInstr::I64GtS(..) => WirInstr::I64LeS,
        // i64 unsigned
        WirInstr::I64LeU(..) => WirInstr::I64GtU,
        WirInstr::I64LtU(..) => WirInstr::I64GeU,
        WirInstr::I64GeU(..) => WirInstr::I64LtU,
        WirInstr::I64GtU(..) => WirInstr::I64LeU,
        // i64 eq/ne
        WirInstr::I64Eq(..) => WirInstr::I64Ne,
        WirInstr::I64Ne(..) => WirInstr::I64Eq,
        _ => return,
    };
    // Extract the two operands from the inner comparison.
    let (l, r) = match inner.as_mut() {
        WirInstr::I32LeS(l, r)
        | WirInstr::I32LtS(l, r)
        | WirInstr::I32GeS(l, r)
        | WirInstr::I32GtS(l, r)
        | WirInstr::I32LeU(l, r)
        | WirInstr::I32LtU(l, r)
        | WirInstr::I32GeU(l, r)
        | WirInstr::I32GtU(l, r)
        | WirInstr::I32Eq(l, r)
        | WirInstr::I32Ne(l, r)
        | WirInstr::I64LeS(l, r)
        | WirInstr::I64LtS(l, r)
        | WirInstr::I64GeS(l, r)
        | WirInstr::I64GtS(l, r)
        | WirInstr::I64LeU(l, r)
        | WirInstr::I64LtU(l, r)
        | WirInstr::I64GeU(l, r)
        | WirInstr::I64GtU(l, r)
        | WirInstr::I64Eq(l, r)
        | WirInstr::I64Ne(l, r) => (
            std::mem::replace(l, Box::new(WirInstr::Nop)),
            std::mem::replace(r, Box::new(WirInstr::Nop)),
        ),
        _ => return,
    };
    *instr = ctor(l, r);
}

/// Fold `if bool { x += 1 }` → `x += bool` (branchless increment).
///
/// Eliminates a conditional branch in the common counting-loop pattern where a
/// boolean value gates an increment-by-one. Since the condition is guaranteed to
/// be 0 or 1, it can be added directly.
fn fold_branchless_increment(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        fold_branchless_increment_in(instr);
    }
}

fn fold_branchless_increment_in(instr: &mut WirInstr) {
    // Recurse into nested blocks first.
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            fold_branchless_increment(body);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            fold_branchless_increment(then_body);
            if let Some(eb) = else_body {
                fold_branchless_increment(eb);
            }
        }
        _ => {}
    }
    // Match: If { cond, then: [LocalSet(x, I32Add(LocalGet(x), I32Const(1)))], else: None }
    let WirInstr::If {
        condition,
        then_body,
        else_body: None,
        result: None,
    } = instr
    else {
        return;
    };
    if then_body.len() != 1 {
        return;
    }
    let WirInstr::LocalSet { name, value } = &then_body[0] else {
        return;
    };
    let WirInstr::I32Add(lhs, rhs) = value.as_ref() else {
        return;
    };
    let WirInstr::LocalGet { name: get_name, .. } = lhs.as_ref() else {
        return;
    };
    if get_name != name {
        return;
    }
    let WirInstr::I32Const(1) = rhs.as_ref() else {
        return;
    };
    if !is_boolean_valued(condition.peel_hint()) {
        return;
    }
    // The fold turns `if cond { x = x + 1 }` into `x = x + cond`, which is
    // only sound when evaluating `cond` does not itself write to `x` —
    // Wasm reads `x` for the LHS of the add before evaluating `cond`,
    // so any in-cond `local.set x` would be clobbered by the post-fold
    // store. This pattern arises from HFS's call-site sync wrapper,
    // whose re-read inserts `local.set _hfs_v` inside an expression
    // when the HFS scalar is being incremented in the if's then-branch.
    if writes_local(condition.peel_hint(), name) {
        return;
    }
    // Transform: x = x + condition. The branch is gone, so a branch hint on
    // the condition is dropped rather than kept around the added operand.
    let mut cond = std::mem::replace(condition, Box::new(WirInstr::Nop));
    cond.take_branch_hint();
    let get = Box::new(WirInstr::LocalGet {
        name: name.clone(),
        result_ty: WirType::I32,
    });
    *instr = WirInstr::LocalSet {
        name: name.clone(),
        value: Box::new(WirInstr::I32Add(get, cond)),
    };
}

/// Returns the upper bound (exclusive) of values this instruction can produce,
/// when that bound is a power of two. Used to drop redundant bitmasks that do
/// not change the value.
///
/// - `I32Load8U` / `I32Load16U` zero-extend from memory and are always in the
///   8-bit / 16-bit unsigned range.
/// - `ArrayGetU` on a packed `u8` / `u16` array field returns a zero-extended
///   byte / short.
/// - `I32And` with a constant mask bounds the result to `mask + 1`.
fn unsigned_bit_width(instr: &WirInstr) -> Option<u32> {
    match instr {
        WirInstr::I32Load8U { .. } => Some(8),
        WirInstr::I32Load16U { .. } => Some(16),
        WirInstr::ArrayGetU {
            result_ty: WirType::U8 | WirType::Bool,
            ..
        } => Some(8),
        WirInstr::ArrayGetU {
            result_ty: WirType::U16,
            ..
        } => Some(16),
        WirInstr::I32Const(v) => {
            if *v >= 0 {
                Some(32 - (*v as u32).leading_zeros())
            } else {
                None
            }
        }
        WirInstr::I32And(l, r) => {
            // Result is bounded by the narrower of the two operands' masks.
            let lw = unsigned_bit_width(l);
            let rw = unsigned_bit_width(r);
            match (lw, rw) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        }
        _ => None,
    }
}

/// Remove redundant `x & ((1 << n) - 1)` masks when `x` is already known to
/// fit in `n` bits (e.g., the result of a zero-extending load or packed array
/// read). The Wasm backend re-introduces truncation on stores into packed
/// storage, so the mask is pure overhead.
fn simplify_redundant_byte_masks(instrs: &mut [WirInstr]) {
    struct Simplify;
    impl WirMutVisitor for Simplify {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            try_drop_mask(instr);
        }
    }
    for instr in instrs.iter_mut() {
        Simplify.visit_instr(instr);
    }
}

fn try_drop_mask(instr: &mut WirInstr) {
    // Match `I32And(expr, I32Const(mask))` or the symmetric form, where `mask`
    // is `2^n - 1` and `expr` already fits in `n` bits.
    let WirInstr::I32And(l, r) = instr else {
        return;
    };
    let (value_box, mask) = match (l.as_ref(), r.as_ref()) {
        (_, WirInstr::I32Const(v)) => (l, *v),
        (WirInstr::I32Const(v), _) => (r, *v),
        _ => return,
    };
    let Some(mask_bits) = power_of_two_minus_one_width(mask) else {
        return;
    };
    let Some(value_bits) = unsigned_bit_width(value_box.as_ref()) else {
        return;
    };
    if value_bits > mask_bits {
        return;
    }
    let value = std::mem::replace(value_box.as_mut(), WirInstr::Nop);
    *instr = value;
}

/// If `v` has the form `2^n - 1` with `1 <= n <= 31`, return `n`.
fn power_of_two_minus_one_width(v: i32) -> Option<u32> {
    if v <= 0 {
        return None;
    }
    let u = v as u32;
    let bits = u.count_ones();
    if bits == u32::BITS - u.leading_zeros() {
        Some(bits)
    } else {
        None
    }
}

/// Returns true if evaluating `instr` (or any of its sub-instructions)
/// performs `local.set` / `local.tee` against the local named
/// `target_name`. Used by `fold_branchless_increment` to refuse the
/// fold when the condition mutates the very local being incremented —
/// the fold relies on `cond` being a pure rvalue.
fn writes_local(instr: &WirInstr, target_name: &str) -> bool {
    if let WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } = instr
        && name == target_name
    {
        return true;
    }
    let mut found = false;
    instr.for_each_child(&mut |child| {
        if !found && writes_local(child, target_name) {
            found = true;
        }
    });
    found
}

/// Returns true if the instruction is guaranteed to produce 0 or 1.
fn is_boolean_valued(instr: &WirInstr) -> bool {
    matches!(
        instr,
        // Integer comparisons
        WirInstr::I32Eq(..)
            | WirInstr::I32Ne(..)
            | WirInstr::I32LtS(..)
            | WirInstr::I32LtU(..)
            | WirInstr::I32GtS(..)
            | WirInstr::I32GtU(..)
            | WirInstr::I32LeS(..)
            | WirInstr::I32LeU(..)
            | WirInstr::I32GeS(..)
            | WirInstr::I32GeU(..)
            | WirInstr::I64Eq(..)
            | WirInstr::I64Ne(..)
            | WirInstr::I64LtS(..)
            | WirInstr::I64LtU(..)
            | WirInstr::I64GtS(..)
            | WirInstr::I64GtU(..)
            | WirInstr::I64LeS(..)
            | WirInstr::I64LeU(..)
            | WirInstr::I64GeS(..)
            | WirInstr::I64GeU(..)
            // Float comparisons
            | WirInstr::F32Eq(..)
            | WirInstr::F32Ne(..)
            | WirInstr::F32Lt(..)
            | WirInstr::F32Gt(..)
            | WirInstr::F32Le(..)
            | WirInstr::F32Ge(..)
            | WirInstr::F64Eq(..)
            | WirInstr::F64Ne(..)
            | WirInstr::F64Lt(..)
            | WirInstr::F64Gt(..)
            | WirInstr::F64Le(..)
            | WirInstr::F64Ge(..)
            // Eqz / null checks
            | WirInstr::I32Eqz(..)
            | WirInstr::I64Eqz(..)
            | WirInstr::RefIsNull(..)
            | WirInstr::RefTest { .. }
            // Bool array element (packed i8 with values 0 or 1)
            | WirInstr::ArrayGet {
                result_ty: WirType::Bool,
                ..
            }
            | WirInstr::ArrayGetU {
                result_ty: WirType::Bool,
                ..
            }
    )
}

/// Fold integer sign-extension instructions into a cheaper equivalent.
///
/// All of these are pure Wasm-instruction-selection rewrites:
/// - `i32.extend8_s(i32.load8_u a)` → `i32.load8_s a` (and the 16-bit variant);
///   the unsigned load + explicit sign-extend collapses into the signed load.
/// - `i32.extend8_s(i32.load8_s a)` → `i32.load8_s a`; re-extending an already
///   sign-extended byte is a no-op (idempotent).
/// - `i32.extend8_s(x & mask)` → `i32.extend8_s(x)` when `mask`'s low 8 bits are
///   all set; `extend8_s` only inspects the low byte, so the mask is dead.
/// - `i32.extend8_s(i32.extend8_s x)` → `i32.extend8_s x` (idempotent), and
///   `i32.extend16_s(i32.extend8_s x)` → `i32.extend8_s x` (the 8-bit result
///   already fits the 16-bit signed range).
fn fold_sign_extensions(instrs: &mut [WirInstr]) {
    struct FoldExt;
    impl WirMutVisitor for FoldExt {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            try_fold_sign_extension(instr);
        }
    }
    for instr in instrs.iter_mut() {
        FoldExt.visit_instr(instr);
    }
}

fn try_fold_sign_extension(instr: &mut WirInstr) {
    // The width (in bits) this sign extension reads from: 8 or 16.
    let width = match instr {
        WirInstr::I32Extend8S(_) => 8,
        WirInstr::I32Extend16S(_) => 16,
        _ => return,
    };
    let (WirInstr::I32Extend8S(inner) | WirInstr::I32Extend16S(inner)) = instr else {
        return;
    };

    // Step 1: drop a redundant `x & mask` whose low `width` bits are all set —
    // the mask cannot change what `extend*_s` observes. Doing this first
    // canonicalises `inner` so step 2 can still see an exposed load/extend
    // (e.g. `extend8_s((load8_u a) & 0xFF)` → `extend8_s(load8_u a)` → folded
    // to `load8_s a` below).
    if let WirInstr::I32And(l, r) = inner.as_mut() {
        let unmasked = match (l.as_ref(), r.as_ref()) {
            (_, WirInstr::I32Const(m)) if low_bits_all_set(*m, width) => Some(l),
            (WirInstr::I32Const(m), _) if low_bits_all_set(*m, width) => Some(r),
            _ => None,
        };
        if let Some(slot) = unmasked {
            let value = std::mem::replace(slot.as_mut(), WirInstr::Nop);
            **inner = value;
        }
    }

    // Step 2: collapse the (possibly newly exposed) inner load/extend.
    match inner.as_mut() {
        // Unsigned load + sign-extend → signed load.
        WirInstr::I32Load8U {
            offset,
            align,
            addr,
        } if width == 8 => {
            let addr = std::mem::replace(addr, Box::new(WirInstr::Nop));
            *instr = WirInstr::I32Load8S {
                offset: *offset,
                align: *align,
                addr,
            };
        }
        WirInstr::I32Load16U {
            offset,
            align,
            addr,
        } if width == 16 => {
            let addr = std::mem::replace(addr, Box::new(WirInstr::Nop));
            *instr = WirInstr::I32Load16S {
                offset: *offset,
                align: *align,
                addr,
            };
        }
        // Re-extending an already-signed value is a no-op: at the same width
        // it is plain idempotence, and `extend16_s` over an 8-bit-extended
        // value leaves it unchanged (the value already lies in `[-128, 127]`,
        // within the 16-bit signed range).
        WirInstr::I32Load8S { .. } | WirInstr::I32Extend8S(_) => {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
        WirInstr::I32Load16S { .. } | WirInstr::I32Extend16S(_) if width == 16 => {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
        _ => {}
    }
}

/// True if the low `width` bits of `mask` are all set (so a `& mask` preserves
/// every bit a `width`-bit sign extension reads).
fn low_bits_all_set(mask: i32, width: u32) -> bool {
    let low = (1u32 << width) - 1;
    (mask.cast_unsigned() & low) == low
}

/// Eliminate redundant `ref.cast` / `ref.test` against the WIR static type.
///
/// - `ref.cast $T(expr)` where `expr` is statically a `$T` reference is the
///   identity. It collapses to `expr`, or to `ref.as_non_null(expr)` when the
///   cast asserts non-null but the source type is nullable.
/// - `ref.test $T(expr)` where `expr` is statically a `$T` reference and the
///   test cannot fail (the source is non-null, or the test accepts null) folds
///   to `1`, provided dropping `expr`'s evaluation is observably safe.
///
/// Only exact `type_id` matches are recognised; supertype/subtype narrowing
/// (the shape variant pattern matching emits) is deliberately left untouched.
fn simplify_redundant_ref_ops(instrs: &mut [WirInstr]) {
    struct SimplifyRef;
    impl WirMutVisitor for SimplifyRef {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            try_simplify_ref_op(instr);
        }
    }
    for instr in instrs.iter_mut() {
        SimplifyRef.visit_instr(instr);
    }
}

/// The statically known GC reference type `(type_id, nullable)` an instruction
/// produces, when the WIR carries enough information to be certain.
fn static_ref_type(instr: &WirInstr) -> Option<(WirTypeId, bool)> {
    match instr {
        WirInstr::LocalGet { result_ty, .. }
        | WirInstr::GlobalGet { result_ty, .. }
        | WirInstr::StructGet { result_ty, .. }
        | WirInstr::ArrayGet { result_ty, .. }
        | WirInstr::ArrayGetS { result_ty, .. }
        | WirInstr::ArrayGetU { result_ty, .. } => match result_ty {
            WirType::Ref { type_id, nullable } => Some((type_id.clone(), *nullable)),
            _ => None,
        },
        WirInstr::StructNew { type_id, .. }
        | WirInstr::ArrayNew { type_id, .. }
        | WirInstr::ArrayNewDefault { type_id, .. }
        | WirInstr::ArrayNewData { type_id, .. }
        | WirInstr::ArrayNewFixed { type_id, .. } => Some((type_id.clone(), false)),
        WirInstr::RefCast {
            type_id, nullable, ..
        } => Some((type_id.clone(), *nullable)),
        WirInstr::RefAsNonNull(inner) => static_ref_type(inner).map(|(t, _)| (t, false)),
        _ => None,
    }
}

fn try_simplify_ref_op(instr: &mut WirInstr) {
    match instr {
        WirInstr::RefCast {
            type_id,
            nullable,
            expr,
        } => {
            let Some((src_id, src_nullable)) = static_ref_type(expr) else {
                return;
            };
            if src_id != *type_id {
                return;
            }
            let inner = std::mem::replace(expr.as_mut(), WirInstr::Nop);
            if !*nullable && src_nullable {
                // The cast still asserts non-null; preserve that as
                // `ref.as_non_null` (cleanup elides it if the source is
                // already statically non-null).
                *instr = WirInstr::RefAsNonNull(Box::new(inner));
            } else {
                *instr = inner;
            }
        }
        WirInstr::RefTest {
            type_id,
            nullable,
            expr,
        } => {
            let Some((src_id, src_nullable)) = static_ref_type(expr) else {
                return;
            };
            // The test always succeeds when the value is exactly `$type_id`
            // and either non-null or the test accepts null.
            let always_true = src_id == *type_id && (!src_nullable || *nullable);
            if always_true && is_side_effect_free(expr) && !may_trap(expr) {
                *instr = WirInstr::I32Const(1);
            }
        }
        _ => {}
    }
}

/// Fuse a `local.set` with the first subsequent `local.get` of the same local
/// into a single `local.tee`.
///
/// When a statement `local.set $x (v)` is immediately followed by a statement
/// whose first-evaluated leaf is `local.get $x`, the pair
/// `(local.set $x v) … (local.get $x …)` collapses to `(local.tee $x v) …`,
/// dropping one `local.get`. The set value `v` is evaluated at the tee site,
/// which is the first thing the consumer statement runs — so nothing executes
/// between the old set and the moved evaluation and the order is preserved.
/// The emptied `local.set` becomes a `Nop`, which the phase-7 `cleanup` removes.
///
/// Restricted to non-reference locals so the tee never interacts with codegen's
/// `ref.as_non_null`-on-`local.tee` narrowing for `ref_locals`. Descent into the
/// consumer stops at control flow (`Block`/`Loop`/`If`/`Seq`/`Br*`/`Select`):
/// these either evaluate their first child conditionally / repeatedly, or list
/// their operands in an order that does not match Wasm's evaluation order.
fn fuse_local_tee(instrs: &mut [WirInstr]) {
    if instrs.len() < 2 {
        return;
    }
    for i in 0..instrs.len() - 1 {
        let WirInstr::LocalSet { name, .. } = &instrs[i] else {
            continue;
        };
        let name = name.clone();
        if !leftmost_leaf_is_local_get(&instrs[i + 1], &name) {
            continue;
        }
        let value = match &mut instrs[i] {
            WirInstr::LocalSet { value, .. } => std::mem::replace(value, Box::new(WirInstr::Nop)),
            _ => unreachable!(),
        };
        let mut slot = Some(value);
        let fused = fuse_into_leftmost_leaf(&mut instrs[i + 1], &name, &mut slot);
        debug_assert!(fused, "leftmost check and fuse must agree");
        instrs[i] = WirInstr::Nop;
    }
}

/// True if the first-evaluated leaf of `instr` is a non-reference
/// `local.get $name`. Mirrors the descent of [`fuse_into_leftmost_leaf`].
fn leftmost_leaf_is_local_get(instr: &WirInstr, name: &str) -> bool {
    if let WirInstr::LocalGet {
        name: got,
        result_ty,
    } = instr
    {
        return got == name && !is_ref_type(result_ty);
    }
    if descent_blocked(instr) {
        return false;
    }
    let mut result = false;
    let mut first = true;
    instr.for_each_child(&mut |child| {
        if first {
            first = false;
            result = leftmost_leaf_is_local_get(child, name);
        }
    });
    result
}

/// Replace the first-evaluated `local.get $name` leaf of `instr` with
/// `local.tee $name (slot)`, taking the value out of `slot`. Returns whether
/// the replacement happened.
fn fuse_into_leftmost_leaf(
    instr: &mut WirInstr,
    name: &str,
    slot: &mut Option<Box<WirInstr>>,
) -> bool {
    if let WirInstr::LocalGet {
        name: got,
        result_ty,
    } = instr
    {
        if got == name
            && !is_ref_type(result_ty)
            && let Some(value) = slot.take()
        {
            *instr = WirInstr::LocalTee {
                name: name.to_string(),
                value,
            };
            return true;
        }
        return false;
    }
    if descent_blocked(instr) {
        return false;
    }
    let mut fused = false;
    let mut first = true;
    instr.for_each_boxed_child_mut(&mut |child| {
        if first {
            first = false;
            fused = fuse_into_leftmost_leaf(child, name, slot);
        }
    });
    fused
}

/// Nodes the leftmost-leaf descent must not enter: control flow whose first
/// child runs conditionally or repeatedly (`Block`/`Loop`/`If`/`Seq`/`Br*`),
/// `Select` (whose Wasm operand order — `if_true`, `if_false`, then condition —
/// differs from the child-visitation order), and `BranchHint` /
/// `MultiValueLocalBind`, whose operands we keep the tee away from.
fn descent_blocked(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Block { .. }
            | WirInstr::Loop { .. }
            | WirInstr::If { .. }
            | WirInstr::Seq(_)
            | WirInstr::Br { .. }
            | WirInstr::BrIf { .. }
            | WirInstr::BrTable { .. }
            | WirInstr::BranchHint { .. }
            | WirInstr::Select { .. }
            | WirInstr::MultiValueLocalBind { .. }
    )
}

fn is_ref_type(ty: &WirType) -> bool {
    matches!(ty, WirType::Ref { .. } | WirType::AbstractRef { .. })
}

/// Relax `LocalGet.result_ty` from non-null to nullable for GC access operands.
///
/// GC access instructions (`array.get`, `array.set`, `struct.get`, `struct.set`,
/// `array.len`, `array.fill`, `array.copy`, `ref.cast`, `ref.test`) accept
/// `(ref null $type)`, so `ref.as_non_null` is unnecessary for their object operand.
/// Codegen emits `ref.as_non_null` only when `result_ty` is non-null, so relaxing
/// it here suppresses the redundant instruction.
fn relax_gc_operand_nullability(instrs: &mut [WirInstr]) {
    struct Relaxer;
    impl WirMutVisitor for Relaxer {
        fn visit_instr(&mut self, instr: &mut WirInstr) {
            self.walk_instr(instr);
            match instr {
                WirInstr::ArrayGet { array, .. }
                | WirInstr::ArrayGetS { array, .. }
                | WirInstr::ArrayGetU { array, .. }
                | WirInstr::ArraySet { array, .. } => relax_ref_local_get(array),
                WirInstr::ArrayLen(a) => relax_ref_local_get(a),
                WirInstr::ArrayFill { array, .. } => relax_ref_local_get(array),
                WirInstr::ArrayCopy { dest, src, .. } => {
                    relax_ref_local_get(dest);
                    relax_ref_local_get(src);
                }
                WirInstr::StructGet { expr, .. } | WirInstr::StructSet { expr, .. } => {
                    relax_ref_local_get(expr);
                }
                WirInstr::RefCast { expr, .. } | WirInstr::RefTest { expr, .. } => {
                    relax_ref_local_get(expr);
                }
                _ => {}
            }
        }
    }
    for instr in instrs.iter_mut() {
        Relaxer.visit_instr(instr);
    }
}

fn relax_ref_local_get(instr: &mut WirInstr) {
    if let WirInstr::LocalGet { result_ty, .. } = instr
        && result_ty.is_nonnull_ref()
    {
        *result_ty = result_ty.clone().as_nullable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn tid(index: u32) -> WirTypeId {
        WirTypeId::new(index, Rc::from(format!("T{index}").as_str()))
    }

    fn local_get(name: &str, result_ty: WirType) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty,
        }
    }

    fn ref_ty(index: u32, nullable: bool) -> WirType {
        WirType::Ref {
            type_id: tid(index),
            nullable,
        }
    }

    #[test]
    fn const_if_folds_through_branch_hint() {
        // A hinted `if` whose condition became constant must still fold; the
        // hint disappears together with the eliminated branch.
        let mut instr = WirInstr::If {
            condition: Box::new(WirInstr::BranchHint {
                likely: false,
                expr: Box::new(WirInstr::I32Const(0)),
            }),
            result: Some(WirType::I32),
            then_body: vec![WirInstr::I32Const(1)],
            else_body: Some(vec![WirInstr::I32Const(7)]),
        };
        eliminate_const_if(&mut instr);
        match &instr {
            WirInstr::Block { body, .. } => {
                assert!(matches!(body.as_slice(), [WirInstr::I32Const(7)]));
            }
            other => panic!("expected folded Block, got {other:?}"),
        }
    }

    #[test]
    fn branchless_increment_folds_through_branch_hint() {
        // `if @unlikely(x < 0) { c = c + 1 }` → `c = c + (x < 0)`; the branch
        // is gone, so the hint wrapper must be dropped, not kept around the
        // added operand.
        let cond = WirInstr::I32LtS(
            Box::new(local_get("x", WirType::I32)),
            Box::new(WirInstr::I32Const(0)),
        );
        let mut instr = WirInstr::If {
            condition: Box::new(WirInstr::BranchHint {
                likely: false,
                expr: Box::new(cond),
            }),
            result: None,
            then_body: vec![WirInstr::LocalSet {
                name: "c".to_string(),
                value: Box::new(WirInstr::I32Add(
                    Box::new(local_get("c", WirType::I32)),
                    Box::new(WirInstr::I32Const(1)),
                )),
            }],
            else_body: None,
        };
        fold_branchless_increment_in(&mut instr);
        let WirInstr::LocalSet { name, value } = &instr else {
            panic!("expected branchless LocalSet, got {instr:?}");
        };
        assert_eq!(name, "c");
        let WirInstr::I32Add(_, rhs) = value.as_ref() else {
            panic!("expected I32Add, got {value:?}");
        };
        assert!(
            matches!(rhs.as_ref(), WirInstr::I32LtS(..)),
            "hint wrapper must be dropped from the folded operand: {rhs:?}"
        );
    }

    #[test]
    fn extend8_over_load8u_becomes_load8s() {
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32Load8U {
            offset: 4,
            align: 0,
            addr: Box::new(WirInstr::I32Const(16)),
        }));
        try_fold_sign_extension(&mut instr);
        match instr {
            WirInstr::I32Load8S { offset, align, .. } => {
                assert_eq!(offset, 4);
                assert_eq!(align, 0);
            }
            other => panic!("expected I32Load8S, got {other:?}"),
        }
    }

    #[test]
    fn extend16_over_load16u_becomes_load16s() {
        let mut instr = WirInstr::I32Extend16S(Box::new(WirInstr::I32Load16U {
            offset: 0,
            align: 1,
            addr: Box::new(WirInstr::I32Const(0)),
        }));
        try_fold_sign_extension(&mut instr);
        assert!(matches!(instr, WirInstr::I32Load16S { .. }));
    }

    #[test]
    fn extend8_over_load8s_is_idempotent() {
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32Load8S {
            offset: 0,
            align: 0,
            addr: Box::new(WirInstr::I32Const(0)),
        }));
        try_fold_sign_extension(&mut instr);
        assert!(matches!(instr, WirInstr::I32Load8S { .. }));
    }

    #[test]
    fn extend8_over_mask_drops_mask() {
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32And(
            Box::new(local_get("x", WirType::I32)),
            Box::new(WirInstr::I32Const(0xFF)),
        )));
        try_fold_sign_extension(&mut instr);
        match instr {
            WirInstr::I32Extend8S(inner) => assert!(matches!(*inner, WirInstr::LocalGet { .. })),
            other => panic!("expected I32Extend8S(LocalGet), got {other:?}"),
        }
    }

    #[test]
    fn extend8_over_masked_load8u_folds_to_load8s() {
        // After the mask is dropped, the exposed `load8_u` must still fold to
        // `load8_s` in the same pass (the re-fold step).
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32And(
            Box::new(WirInstr::I32Load8U {
                offset: 0,
                align: 0,
                addr: Box::new(WirInstr::I32Const(0)),
            }),
            Box::new(WirInstr::I32Const(0xFF)),
        )));
        try_fold_sign_extension(&mut instr);
        assert!(matches!(instr, WirInstr::I32Load8S { .. }));
    }

    #[test]
    fn extend8_over_extend16_is_untouched() {
        // `extend8_s(extend16_s x)` truncates to 8 bits; it must NOT collapse
        // to the wider inner `extend16_s x`.
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32Extend16S(Box::new(
            local_get("x", WirType::I32),
        ))));
        try_fold_sign_extension(&mut instr);
        match instr {
            WirInstr::I32Extend8S(inner) => assert!(matches!(*inner, WirInstr::I32Extend16S(_))),
            other => panic!("expected unchanged I32Extend8S(I32Extend16S), got {other:?}"),
        }
    }

    #[test]
    fn extend8_over_load16s_is_untouched() {
        // `extend8_s(load16_s a)` truncates to 8 bits; must NOT fold to load16_s.
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32Load16S {
            offset: 0,
            align: 0,
            addr: Box::new(WirInstr::I32Const(0)),
        }));
        try_fold_sign_extension(&mut instr);
        match instr {
            WirInstr::I32Extend8S(inner) => assert!(matches!(*inner, WirInstr::I32Load16S { .. })),
            other => panic!("expected unchanged I32Extend8S(I32Load16S), got {other:?}"),
        }
    }

    #[test]
    fn extend8_keeps_partial_low_mask() {
        // Mask 0x0F does not preserve the full low byte, so it must stay.
        let mut instr = WirInstr::I32Extend8S(Box::new(WirInstr::I32And(
            Box::new(local_get("x", WirType::I32)),
            Box::new(WirInstr::I32Const(0x0F)),
        )));
        try_fold_sign_extension(&mut instr);
        assert!(matches!(*as_extend_inner(&instr), WirInstr::I32And(..)));
    }

    fn as_extend_inner(instr: &WirInstr) -> &WirInstr {
        match instr {
            WirInstr::I32Extend8S(inner) | WirInstr::I32Extend16S(inner) => inner,
            _ => panic!("not an extend"),
        }
    }

    #[test]
    fn refcast_same_type_nullable_collapses_to_expr() {
        // ref.cast (nullable) of an already non-null T -> just the expr.
        let mut instr = WirInstr::RefCast {
            type_id: tid(7),
            nullable: true,
            expr: Box::new(local_get("p", ref_ty(7, false))),
        };
        try_simplify_ref_op(&mut instr);
        assert!(matches!(instr, WirInstr::LocalGet { .. }));
    }

    #[test]
    fn refcast_nonnull_of_nullable_source_keeps_assertion() {
        // ref.cast (non-null) of a nullable T -> ref.as_non_null(expr).
        let mut instr = WirInstr::RefCast {
            type_id: tid(7),
            nullable: false,
            expr: Box::new(local_get("p", ref_ty(7, true))),
        };
        try_simplify_ref_op(&mut instr);
        assert!(matches!(instr, WirInstr::RefAsNonNull(_)));
    }

    #[test]
    fn refcast_different_type_is_untouched() {
        let mut instr = WirInstr::RefCast {
            type_id: tid(7),
            nullable: false,
            expr: Box::new(local_get("p", ref_ty(9, false))),
        };
        try_simplify_ref_op(&mut instr);
        assert!(matches!(instr, WirInstr::RefCast { .. }));
    }

    #[test]
    fn reftest_always_true_folds_to_one() {
        let mut instr = WirInstr::RefTest {
            type_id: tid(7),
            nullable: false,
            expr: Box::new(local_get("p", ref_ty(7, false))),
        };
        try_simplify_ref_op(&mut instr);
        assert!(matches!(instr, WirInstr::I32Const(1)));
    }

    #[test]
    fn reftest_nonnull_of_nullable_source_is_untouched() {
        // A non-null test of a nullable source can still fail on null.
        let mut instr = WirInstr::RefTest {
            type_id: tid(7),
            nullable: false,
            expr: Box::new(local_get("p", ref_ty(7, true))),
        };
        try_simplify_ref_op(&mut instr);
        assert!(matches!(instr, WirInstr::RefTest { .. }));
    }

    #[test]
    fn tee_fuses_set_then_first_get() {
        // local.set t = (x + 1); return t * t  ->  return (local.tee t (x+1)) * t
        let mut body = vec![
            WirInstr::LocalSet {
                name: "t".to_string(),
                value: Box::new(WirInstr::I32Add(
                    Box::new(local_get("x", WirType::I32)),
                    Box::new(WirInstr::I32Const(1)),
                )),
            },
            WirInstr::Return {
                value: Some(Box::new(WirInstr::I32Mul(
                    Box::new(local_get("t", WirType::I32)),
                    Box::new(local_get("t", WirType::I32)),
                ))),
            },
        ];
        fuse_local_tee(&mut body);
        assert!(matches!(body[0], WirInstr::Nop));
        let WirInstr::Return { value: Some(v) } = &body[1] else {
            panic!("expected return");
        };
        let WirInstr::I32Mul(lhs, rhs) = v.as_ref() else {
            panic!("expected mul");
        };
        assert!(matches!(lhs.as_ref(), WirInstr::LocalTee { .. }));
        assert!(matches!(rhs.as_ref(), WirInstr::LocalGet { .. }));
    }

    #[test]
    fn tee_fuses_when_set_value_reads_same_local() {
        // local.set t = (t + 1); return t * t
        // The set value reads `t`; the tee evaluates it with the old `t`, then
        // stores, and the trailing `t` read observes the new value.
        let mut body = vec![
            WirInstr::LocalSet {
                name: "t".to_string(),
                value: Box::new(WirInstr::I32Add(
                    Box::new(local_get("t", WirType::I32)),
                    Box::new(WirInstr::I32Const(1)),
                )),
            },
            WirInstr::Return {
                value: Some(Box::new(WirInstr::I32Mul(
                    Box::new(local_get("t", WirType::I32)),
                    Box::new(local_get("t", WirType::I32)),
                ))),
            },
        ];
        fuse_local_tee(&mut body);
        assert!(matches!(body[0], WirInstr::Nop));
        let WirInstr::Return { value: Some(v) } = &body[1] else {
            panic!("expected return");
        };
        let WirInstr::I32Mul(lhs, _) = v.as_ref() else {
            panic!("expected mul");
        };
        // The leftmost `t` becomes the tee; the inner `t + 1` is preserved.
        let WirInstr::LocalTee { value, .. } = lhs.as_ref() else {
            panic!("expected leftmost local.tee");
        };
        assert!(matches!(value.as_ref(), WirInstr::I32Add(..)));
    }

    #[test]
    fn tee_does_not_fuse_when_get_is_not_leftmost() {
        // return 1 * t : the first leaf is the constant, not `t`.
        let mut body = vec![
            WirInstr::LocalSet {
                name: "t".to_string(),
                value: Box::new(WirInstr::I32Const(5)),
            },
            WirInstr::Return {
                value: Some(Box::new(WirInstr::I32Mul(
                    Box::new(WirInstr::I32Const(1)),
                    Box::new(local_get("t", WirType::I32)),
                ))),
            },
        ];
        fuse_local_tee(&mut body);
        assert!(matches!(body[0], WirInstr::LocalSet { .. }));
    }

    #[test]
    fn tee_does_not_fuse_ref_locals() {
        let mut body = vec![
            WirInstr::LocalSet {
                name: "r".to_string(),
                value: Box::new(WirInstr::RefNull {
                    heap_type: crate::wir::WirAbstractHeapType::Any,
                }),
            },
            WirInstr::Return {
                value: Some(Box::new(local_get("r", ref_ty(1, true)))),
            },
        ];
        fuse_local_tee(&mut body);
        assert!(matches!(body[0], WirInstr::LocalSet { .. }));
    }

    #[test]
    fn tee_does_not_descend_into_loop() {
        // A `local.get` inside a loop body could re-execute; never fuse across it.
        let mut body = vec![
            WirInstr::LocalSet {
                name: "t".to_string(),
                value: Box::new(WirInstr::I32Const(0)),
            },
            WirInstr::Loop {
                label: None,
                body: vec![WirInstr::Drop(Box::new(local_get("t", WirType::I32)))],
            },
        ];
        fuse_local_tee(&mut body);
        assert!(matches!(body[0], WirInstr::LocalSet { .. }));
    }
}
