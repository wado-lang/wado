//! Peephole optimization pass for WIR.
//!
//! Per-function pass that applies local rewrites:
//! - Constant folding on integer comparisons
//! - Dead `If` elimination (constant condition)
//! - Redundant `ValueCopy` elision
//! - Copy-used-only-for-field-reads elision

use crate::wir::{WirInstr, WirPackage, WirType, WirTypeDef};
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

fn propagate_copies_in_function(body: &mut Vec<WirInstr>, params: &IndexSet<String>) {
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
                Some(next) => current = next.clone(),
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
    relax_gc_operand_nullability(instrs);
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

/// Try to evaluate a WIR condition to a boolean constant.
fn try_fold_wir_to_bool(instr: &WirInstr) -> Option<bool> {
    match instr {
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
    if !is_boolean_valued(condition.as_ref()) {
        return;
    }
    // The fold turns `if cond { x = x + 1 }` into `x = x + cond`, which is
    // only sound when evaluating `cond` does not itself write to `x` —
    // Wasm reads `x` for the LHS of the add before evaluating `cond`,
    // so any in-cond `local.set x` would be clobbered by the post-fold
    // store. This pattern arises from HFS's call-site sync wrapper,
    // whose re-read inserts `local.set _hfs_v` inside an expression
    // when the HFS scalar is being incremented in the if's then-branch.
    if writes_local(condition.as_ref(), name) {
        return;
    }
    // Transform: x = x + condition
    let cond = std::mem::replace(condition, Box::new(WirInstr::Nop));
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
