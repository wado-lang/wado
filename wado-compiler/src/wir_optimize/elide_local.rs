//! Write-only local elimination pass for WIR.
//!
//! Converts `LocalSet(x, expr)` to `Drop(expr)` or `Nop` when local `x` is never
//! read, cleaning up temporaries left by other passes.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule};
use crate::wir_visitor::WirMutVisitor;

use super::util::{collect_local_gets_deep, is_side_effect_free};

pub(super) fn elide_write_only_locals(module: &mut WirModule) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            loop {
                if !elide_write_only_locals_in_body(body) {
                    break;
                }
            }
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
        // Only recurse into bodies (Block/Loop/If/Seq), not expression children.
        // LocalSet only appears at body level, so expression children are skipped.
        match instr {
            WirInstr::Block { body, .. }
            | WirInstr::Loop { body, .. }
            | WirInstr::Seq(body) => self.visit_body(body),
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
