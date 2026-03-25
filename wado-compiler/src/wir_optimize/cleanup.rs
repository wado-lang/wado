//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, and dead code after
//! `Unreachable`. Called multiple times throughout the pipeline as an interpass
//! utility rather than a standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirModule, WirTypeDef};

pub(super) fn cleanup(module: &mut WirModule) {
    // Take ownership of types temporarily to allow mutable borrow of functions.
    let types = std::mem::take(&mut module.types);
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
            eliminate_dead_locals(body);
            // Collect locals (DeclareLocal + params) declared as non-null ref types.
            // RefAsNonNull(LocalGet(name)) is redundant for these at the WIR level.
            let mut nonnull_locals = collect_nonnull_ref_locals(body);
            if let Some(WirTypeDef::Func(ft)) = types.get(func.type_id.index() as usize) {
                for (param_ty, param_name) in ft.params.iter().zip(func.param_names.iter()) {
                    if param_ty.is_nonnull_ref() {
                        nonnull_locals.insert(param_name.clone());
                    }
                }
            }
            cleanup_instrs(body, &nonnull_locals, &types);
        }
    }
    module.types = types;
}

/// Collect local names declared via `DeclareLocal` with a non-null ref type.
fn collect_nonnull_ref_locals(body: &[WirInstr]) -> IndexSet<String> {
    let mut nonnull_locals = IndexSet::default();
    for instr in body {
        collect_nonnull_ref_locals_instr(instr, &mut nonnull_locals);
    }
    nonnull_locals
}

fn collect_nonnull_ref_locals_instr(instr: &WirInstr, nonnull_locals: &mut IndexSet<String>) {
    match instr {
        WirInstr::DeclareLocal { name, ty } if ty.is_nonnull_ref() => {
            nonnull_locals.insert(name.clone());
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for i in body {
                collect_nonnull_ref_locals_instr(i, nonnull_locals);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for i in then_body {
                collect_nonnull_ref_locals_instr(i, nonnull_locals);
            }
            if let Some(eb) = else_body {
                for i in eb {
                    collect_nonnull_ref_locals_instr(i, nonnull_locals);
                }
            }
        }
        _ => {}
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

fn cleanup_instrs(
    instrs: &mut Vec<WirInstr>,
    nonnull_locals: &IndexSet<String>,
    types: &[WirTypeDef],
) {
    for instr in instrs.iter_mut() {
        cleanup_instr(instr, nonnull_locals, types);
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

fn cleanup_instr(instr: &mut WirInstr, nonnull_locals: &IndexSet<String>, types: &[WirTypeDef]) {
    // Recurse into nested bodies first (bottom-up).
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            cleanup_instrs(body, nonnull_locals, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            cleanup_instr(condition, nonnull_locals, types);
            cleanup_instrs(then_body, nonnull_locals, types);
            if let Some(eb) = else_body {
                cleanup_instrs(eb, nonnull_locals, types);
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                cleanup_instr(child, nonnull_locals, types)
            });
        }
    }
    // Elide redundant RefAsNonNull when the inner expression is already non-null:
    // - non-null-producing instructions (StructNew, ArrayNew, etc.)
    // - LocalGet of a local/param declared as a non-null ref type
    // - StructGet of a struct field declared as a non-null ref type
    if let WirInstr::RefAsNonNull(inner) = instr {
        let is_nonnull = inner.is_nonnull_result()
            || matches!(inner.as_ref(), WirInstr::LocalGet { name } if nonnull_locals.contains(name.as_str()))
            || is_nonnull_struct_get(inner, types);
        if is_nonnull {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
    }
}

/// Returns true if `instr` is a `StructGet` whose field is declared as a non-null ref type.
fn is_nonnull_struct_get(instr: &WirInstr, types: &[WirTypeDef]) -> bool {
    if let WirInstr::StructGet {
        type_id, field_name, ..
    } = instr
        && let Some(WirTypeDef::Struct(st)) = types.get(type_id.index() as usize)
    {
        return st
            .fields
            .iter()
            .any(|f| f.name == *field_name && f.ty.is_nonnull_ref());
    }
    false
}
