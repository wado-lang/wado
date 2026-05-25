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
//! v)` whose `x` is never read in the function body, the assignment is
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

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::util::{collect_local_gets_deep, is_side_effect_free, may_trap};

pub(super) fn elide_write_only_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            while elide_write_only_locals_in_body(body) {}
        }
    }
}

fn elide_write_only_locals_in_body(body: &mut [WirInstr]) -> bool {
    let mut read_locals: IndexSet<String> = IndexSet::default();
    for instr in body.iter() {
        collect_local_gets_deep(instr, &mut read_locals);
    }

    let mut visitor = ElideWriteOnly {
        read_locals: &read_locals,
        changed: false,
    };
    for instr in body.iter_mut() {
        visitor.visit_instr(instr);
    }
    visitor.changed
}

struct ElideWriteOnly<'a> {
    read_locals: &'a IndexSet<String>,
    changed: bool,
}

impl WirMutVisitor for ElideWriteOnly<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::LocalSet { name, value } = instr
            && !self.read_locals.contains(name.as_str())
        {
            let value_expr = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if is_side_effect_free(&value_expr) {
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
        // Only recurse into bodies (Block/Loop/If/Seq), not expression
        // children. `LocalSet` only appears at body level, so descending
        // through expression operands wastes work.
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                self.visit_body(body);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                self.visit_body(then_body);
                if let Some(eb) = else_body {
                    self.visit_body(eb);
                }
            }
            _ => {}
        }
    }
}
