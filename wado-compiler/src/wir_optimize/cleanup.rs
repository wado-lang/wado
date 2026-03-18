//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, and dead code after
//! `Unreachable`. Called multiple times throughout the pipeline as an interpass
//! utility rather than a standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule};

pub(super) fn cleanup(module: &mut WirModule) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
            eliminate_dead_locals(body);
            cleanup_instrs(body);
        }
    }
}

/// Remove `DeclareLocal` instructions for locals that are never referenced
/// by any `LocalGet`, `LocalSet`, `LocalTee`, or `MultiValueLocalBind`.
fn eliminate_dead_locals(body: &mut [WirInstr]) {
    let mut used: IndexSet<String> = IndexSet::default();
    for instr in body.iter() {
        collect_local_uses(instr, &mut used);
    }
    for instr in body.iter_mut() {
        nop_unused_declare_locals(instr, &used);
    }
}

fn collect_local_uses(instr: &WirInstr, used: &mut IndexSet<String>) {
    match instr {
        WirInstr::LocalGet { name } => {
            used.insert(name.clone());
        }
        WirInstr::LocalSet { name, value } => {
            used.insert(name.clone());
            collect_local_uses(value, used);
        }
        WirInstr::LocalTee { name, value } => {
            used.insert(name.clone());
            collect_local_uses(value, used);
        }
        WirInstr::MultiValueLocalBind { instr, locals } => {
            collect_local_uses(instr, used);
            for local in locals.iter().flatten() {
                used.insert(local.clone());
            }
        }
        WirInstr::DeclareLocal { .. } | WirInstr::Nop => {}
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body {
                collect_local_uses(i, used);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            collect_local_uses(condition, used);
            for i in then_body {
                collect_local_uses(i, used);
            }
            if let Some(eb) = else_body {
                for i in eb {
                    collect_local_uses(i, used);
                }
            }
        }
        _ => {
            instr.for_each_child(&mut |child| collect_local_uses(child, used));
        }
    }
}

fn nop_unused_declare_locals(instr: &mut WirInstr, used: &IndexSet<String>) {
    match instr {
        WirInstr::DeclareLocal { name, .. } => {
            if !used.contains(name.as_str()) {
                *instr = WirInstr::Nop;
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body {
                nop_unused_declare_locals(i, used);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for i in then_body {
                nop_unused_declare_locals(i, used);
            }
            if let Some(eb) = else_body {
                for i in eb {
                    nop_unused_declare_locals(i, used);
                }
            }
        }
        _ => {}
    }
}

fn cleanup_instrs(instrs: &mut Vec<WirInstr>) {
    for instr in instrs.iter_mut() {
        cleanup_instr(instr);
    }
    // Remove nops.
    instrs.retain(|i| !matches!(i, WirInstr::Nop));
    // Truncate after first unreachable (dead code elimination).
    if let Some(pos) = instrs
        .iter()
        .position(|i| matches!(i, WirInstr::Unreachable))
    {
        instrs.truncate(pos + 1);
    }
}

fn cleanup_instr(instr: &mut WirInstr) {
    // Recurse into nested bodies first (bottom-up).
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            cleanup_instrs(body);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            cleanup_instr(condition);
            cleanup_instrs(then_body);
            if let Some(eb) = else_body {
                cleanup_instrs(eb);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| cleanup_instr(child));
        }
    }
    // Elide redundant RefAsNonNull wrapping a non-null-producing instruction.
    if let WirInstr::RefAsNonNull(inner) = instr
        && inner.is_nonnull_result()
    {
        *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
    }
}
