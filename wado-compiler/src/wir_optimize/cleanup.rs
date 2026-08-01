//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, and dead code after
//! `Unreachable`. Called multiple times throughout the pipeline as an interpass
//! utility rather than a standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

pub(super) fn cleanup(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
            eliminate_dead_locals(body);
            let mut visitor = CleanupVisitor;
            visitor.visit_body(body);
        }
    }
}

/// Elide the redundant `RefAsNonNull` wrappers `struct_new` adds for
/// non-nullable fields from every global initializer. The emitter drops them
/// from a const expression anyway; stripping keeps the IR canonical and lets
/// `dedupe_const_globals` compare inits structurally.
pub(super) fn cleanup_global_inits(module: &mut WirPackage) {
    for global in &mut module.globals {
        CleanupVisitor.visit_instr(&mut global.init);
    }
}

/// Remove `DeclareLocal` instructions for locals that are never referenced
/// by any `LocalGet`, `LocalSet`, `LocalTee`, or `MultiValueLocalBind`.
fn eliminate_dead_locals(body: &mut [WirInstr]) {
    let mut used: IndexSet<String> = IndexSet::default();
    let mut collector = CollectLocalUses { used: &mut used };
    for instr in body.iter() {
        collector.visit_instr(instr);
    }
    let mut noper = NopUnusedDeclareLocals { used: &used };
    for instr in body.iter_mut() {
        noper.visit_instr(instr);
    }
}

struct CollectLocalUses<'a> {
    used: &'a mut IndexSet<String>,
}

impl WirRefVisitor for CollectLocalUses<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalGet { name, .. }
            | WirInstr::LocalSet { name, .. }
            | WirInstr::LocalTee { name, .. } => {
                self.used.insert(name.clone());
            }
            WirInstr::MultiValueLocalBind { locals, .. } => {
                for local in locals.iter().flatten() {
                    self.used.insert(local.clone());
                }
            }
            _ => {}
        }
        self.walk_instr(instr);
    }
}

struct NopUnusedDeclareLocals<'a> {
    used: &'a IndexSet<String>,
}

impl WirMutVisitor for NopUnusedDeclareLocals<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::DeclareLocal { name, .. } = instr
            && !self.used.contains(name.as_str())
        {
            *instr = WirInstr::Nop;
            return;
        }
        self.walk_instr(instr);
    }
}

struct CleanupVisitor;

impl WirMutVisitor for CleanupVisitor {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        // Remove nops.
        body.retain(|i| !matches!(i, WirInstr::Nop));
        // Truncate after first unreachable (dead code elimination).
        if let Some(pos) = body.iter().position(|i| matches!(i, WirInstr::Unreachable)) {
            body.truncate(pos + 1);
        }
    }

    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        // Post-visit: elide redundant RefAsNonNull when the inner expression is already non-null.
        if let WirInstr::RefAsNonNull(inner) = instr
            && inner.is_nonnull_result()
        {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
    }
}
