//! Write-only local elimination pass for WIR.
//!
//! Converts `LocalSet(x, expr)` to `Drop(expr)` or `Nop` when local `x` is never
//! read, cleaning up temporaries left by other passes.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule};

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

fn elide_write_only_locals_in_body(body: &mut Vec<WirInstr>) -> bool {
    let mut read_locals: IndexSet<String> = IndexSet::default();
    for instr in body.iter() {
        collect_local_gets_deep(instr, &mut read_locals);
    }

    let mut changed = false;
    for instr in body.iter_mut() {
        elide_write_only_in_instr(instr, &read_locals, &mut changed);
    }
    changed
}

fn elide_write_only_in_instr(
    instr: &mut WirInstr,
    read_locals: &IndexSet<String>,
    changed: &mut bool,
) {
    match instr {
        WirInstr::LocalSet { name, value } if !read_locals.contains(name.as_str()) => {
            let value_expr = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if is_side_effect_free(&value_expr) {
                *instr = WirInstr::Nop;
            } else {
                *instr = WirInstr::Drop(Box::new(value_expr));
            }
            *changed = true;
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                elide_write_only_in_instr(child, read_locals, changed);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter_mut() {
                elide_write_only_in_instr(child, read_locals, changed);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    elide_write_only_in_instr(child, read_locals, changed);
                }
            }
        }
        _ => {}
    }
}
