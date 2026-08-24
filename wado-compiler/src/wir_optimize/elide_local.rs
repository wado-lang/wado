//! Write-only local elimination for the locals `wir_build` synthesises, whose
//! names do not exist at TIR for `optimize::elide_local` to see. A
//! `LocalSet(x, v)` whose `x` is never read elsewhere loses its store: the whole
//! statement when `v` is side-effect-free, else a bare `Drop(v)`. Runs to a
//! fixed point, since eliding one write can strand another.

use crate::hashmap::IndexMap;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::nullability::Nullability;
use super::util::{count_local_gets, is_side_effect_free, may_trap_in};

pub(super) fn elide_write_only_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        // Declared locals are stable across the sweep (elision only nops stores;
        // `DeclareLocal`s go later in `cleanup`), so read the SSoT once.
        let locals = func.declared_locals();
        if let Some(body) = &mut func.body {
            let null = Nullability::new(&locals);
            while elide_write_only_locals_in_body(body, &null) {}
        }
    }
}

fn elide_write_only_locals_in_body(body: &mut [WirInstr], null: &Nullability) -> bool {
    let mut read_counts: IndexMap<String, u32> = IndexMap::default();
    for instr in body.iter() {
        count_local_gets(instr, &mut read_counts);
    }

    let mut visitor = ElideWriteOnly {
        read_counts: &read_counts,
        null,
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
    null: &'a Nullability<'a>,
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
            if is_side_effect_free(&value_expr) && !may_trap_in(&value_expr, self.null) {
                *instr = WirInstr::Nop;
            } else {
                *instr = WirInstr::Drop(Box::new(value_expr));
            }
            self.changed = true;
            return;
        }
        // `drop(side_effect_free_expr)` is dead — the value is discarded by
        // definition. Catches the statement-position `Expr(struct.new …)` /
        // `Expr(self.field)` shapes the TIR pass leaves alone, not knowing what
        // they were before lowering. At WIR level every effect a call had is
        // already its own node, so a `is_side_effect_free` sub-tree is dead.
        if let WirInstr::Drop(value) = instr
            && is_side_effect_free(value)
            && !may_trap_in(value, self.null)
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
    use crate::wir::{WirLocals, WirType};
    use std::assert_matches;

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
        assert!(elide_write_only_locals_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        assert_matches!(body[0], WirInstr::Nop);
    }

    /// A read outside the store's own RHS keeps it.
    #[test]
    fn externally_read_store_is_kept() {
        let mut body = vec![increment("x"), lset("sink", lget("x"))];
        assert!(elide_write_only_locals_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        assert_matches!(
            &body[0],
            WirInstr::LocalSet { name, .. } if name == "x",
            "x is read by the sink copy and must survive"
        );
        // The sink itself is write-only and goes.
        assert_matches!(body[1], WirInstr::Nop);
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
        assert!(elide_write_only_locals_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        let WirInstr::LocalSet { value, .. } = &body[0] else {
            panic!("z is read by the return and must survive");
        };
        let WirInstr::If { then_body, .. } = value.as_ref() else {
            panic!("expected if-expression value");
        };
        assert_matches!(
            then_body[0],
            WirInstr::Nop,
            "write-only w inside the arm must elide"
        );
    }
}
