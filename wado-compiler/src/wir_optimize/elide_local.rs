//! Write-only local elimination pass for WIR.
//!
//! The TIR-level `optimize::elide_local` covers locals that originate at
//! TIR (user `let`, SROA / variant-lowering shadow temps, etc.). It can't
//! see locals that the WIR builder synthesises during lowering — match
//! scrutinee temps (`__match_scrut_N`), multi-value temps, the
//! `__pair_temp_N` pair Future / Stream `new` returns into, and so on —
//! because those names don't exist at TIR. Once `wir_build` runs, those
//! locals can become write-only when the surrounding lowering shape
//! turns out not to need their value (e.g. every match arm has a
//! wildcard / binding pattern, so nothing reads `__match_scrut_N`).
//!
//! This pass cleans those up after `wir_build`. For each `LocalSet(x,
//! v)` whose `x` is never read in the function body — reads inside this
//! store's own `v` don't count, so a self-referencing dead store
//! `x = x + 1` with no other reads still elides — the assignment is
//! rewritten:
//!
//! - `v` has no observable side effects → drop the whole `LocalSet`.
//! - `v` has observable side effects → replace with `Drop(v)` so the
//!   side effects still run, but the dead store is gone.
//!
//! Recurses to a fixed point so that eliding one write doesn't strand a
//! second write that was only kept alive by a (now-eliminated) read of
//! the first. The matching `DeclareLocal` is taken out by the
//! subsequent `cleanup` pass.
//!
//! Locals that *are* read remain untouched — this pass deliberately does
//! not subsume copy propagation or constant propagation; it's a narrow
//! cleanup pass for write-only locals only.

use crate::hashmap::IndexMap;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::util::{count_local_gets, is_side_effect_free, may_trap};

pub(super) fn elide_write_only_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            while elide_write_only_locals_in_body(body) {}
        }
    }
}

fn elide_write_only_locals_in_body(body: &mut [WirInstr]) -> bool {
    let mut read_counts: IndexMap<String, u32> = IndexMap::default();
    for instr in body.iter() {
        count_local_gets(instr, &mut read_counts);
    }

    let mut visitor = ElideWriteOnly {
        read_counts: &read_counts,
        changed: false,
    };
    for instr in body.iter_mut() {
        visitor.visit_instr(instr);
    }
    visitor.changed
}

struct ElideWriteOnly<'a> {
    /// Whole-body `LocalGet` counts, collected before this sweep. Elisions
    /// during the sweep only remove reads, so a stale count over-approximates
    /// — a store can be kept a round too long, never wrongly elided; the
    /// fixed-point loop converges the leftovers.
    read_counts: &'a IndexMap<String, u32>,
    changed: bool,
}

impl ElideWriteOnly<'_> {
    /// Is `name` read anywhere outside `own_value` (this store's RHS)?
    /// A store whose only reads sit in its own RHS is still write-only:
    /// dropping it drops those reads with it.
    fn is_read_elsewhere(&self, name: &str, own_value: &WirInstr) -> bool {
        let total = self.read_counts.get(name).copied().unwrap_or(0);
        if total == 0 {
            return false;
        }
        let mut own_counts: IndexMap<String, u32> = IndexMap::default();
        count_local_gets(own_value, &mut own_counts);
        total > own_counts.get(name).copied().unwrap_or(0)
    }
}

impl WirMutVisitor for ElideWriteOnly<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::LocalSet { name, value } = instr
            && !self.is_read_elsewhere(name, value)
        {
            let value_expr = std::mem::replace(value.as_mut(), WirInstr::Nop);
            // Preserve `may_trap` sub-trees (OOB array/memory reads,
            // null receiver, div-by-zero, …) — Wado language semantics
            // requires those traps to fire even when the binding
            // target is unused. Drops them through a `Drop` so the
            // peephole-level `Drop(may_trap)` guard in path 2 below
            // keeps them out of harm's way.
            if is_side_effect_free(&value_expr) && !may_trap(&value_expr) {
                *instr = WirInstr::Nop;
            } else {
                *instr = WirInstr::Drop(Box::new(value_expr));
            }
            self.changed = true;
            return;
        }
        // `drop(side_effect_free_expr)` is dead — the dropped value is
        // discarded by definition, so a sub-tree with no observable
        // effect contributes nothing. Catches the
        // `Expr(struct.new T { ... })` /
        // `Expr(self.field)` shapes the TIR-level `elide_local` cannot
        // remove in stmt position (those Exprs may have started life as
        // `Expr(Call(...))` or labeled-block remnants of a
        // `stores`-annotated call, and the TIR pass conservatively
        // leaves them be — see the `stores_optimize_mixed_calls`
        // regression test). At WIR level, every effect the call ever
        // had is already in the WIR shape (`Call` / `LocalSet` /
        // `StructSet` / ...), so a residual `Drop` whose sub-tree
        // satisfies `is_side_effect_free` is genuinely dead.
        if let WirInstr::Drop(value) = instr
            && is_side_effect_free(value)
            && !may_trap(value)
        {
            *instr = WirInstr::Nop;
            self.changed = true;
            return;
        }
        // Full recursion: statement lists also hide in value positions (an
        // if-expression arm inside a `LocalSet` value, a `Seq` inside a call
        // argument), and a write-only `LocalSet` there is just as dead.
        self.walk_instr(instr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::WirType;

    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    fn lset(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }
    fn increment(name: &str) -> WirInstr {
        lset(
            name,
            WirInstr::I32Add(Box::new(lget(name)), Box::new(WirInstr::I32Const(1))),
        )
    }

    /// `x = x + 1` with no other reads of `x` is a dead store even though its
    /// own RHS reads `x`.
    #[test]
    fn self_referencing_dead_store_elides() {
        let mut body = vec![increment("x")];
        assert!(elide_write_only_locals_in_body(&mut body));
        assert!(matches!(body[0], WirInstr::Nop));
    }

    /// A read outside the store's own RHS keeps it.
    #[test]
    fn externally_read_store_is_kept() {
        let mut body = vec![increment("x"), lset("sink", lget("x"))];
        assert!(elide_write_only_locals_in_body(&mut body));
        assert!(
            matches!(&body[0], WirInstr::LocalSet { name, .. } if name == "x"),
            "x is read by the sink copy and must survive: {:?}",
            body[0]
        );
        // The sink itself is write-only and goes.
        assert!(matches!(body[1], WirInstr::Nop));
    }

    /// A write-only local buried in a value-position statement list (an
    /// if-expression arm) is reachable and elided.
    #[test]
    fn write_only_local_in_if_expression_arm_elides() {
        let mut body = vec![
            lset(
                "z",
                WirInstr::If {
                    condition: Box::new(lget("c")),
                    result: Some(WirType::I32),
                    then_body: vec![lset("w", WirInstr::I32Const(5)), WirInstr::I32Const(1)],
                    else_body: Some(vec![WirInstr::I32Const(2)]),
                },
            ),
            WirInstr::Return {
                value: Some(Box::new(lget("z"))),
            },
        ];
        assert!(elide_write_only_locals_in_body(&mut body));
        let WirInstr::LocalSet { value, .. } = &body[0] else {
            panic!("z is read by the return and must survive");
        };
        let WirInstr::If { then_body, .. } = value.as_ref() else {
            panic!("expected if-expression value");
        };
        assert!(
            matches!(then_body[0], WirInstr::Nop),
            "write-only w inside the arm must elide: {:?}",
            then_body[0]
        );
    }
}
